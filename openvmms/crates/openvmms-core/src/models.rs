use anyhow::Context;
use anyhow::bail;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

pub const DEFAULT_DAEMON_PORT: u16 = 47620;
pub const DEFAULT_DAEMON_LISTEN: &str = "127.0.0.1:47620";
pub const MAX_VM_MEMORY_MB: u64 = 16 * 1024 * 1024;

/// Parses and validates a loopback HTTP listen address (e.g. `127.0.0.1:47620`,
/// `[::1]:47620`, or `http://localhost:47620`), rejecting arbitrary hostnames and
/// non-loopback IP addresses.
pub fn parse_loopback_listen_addr(raw: &str) -> anyhow::Result<std::net::SocketAddr> {
    let trimmed = raw.trim();
    let without_scheme = if trimmed
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
    {
        &trimmed[7..]
    } else if trimmed
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
    {
        &trimmed[8..]
    } else {
        trimmed
    };
    let host_port = without_scheme.trim_end_matches('/');
    if let Some(port_str) = host_port
        .get(..10)
        .filter(|p| p.eq_ignore_ascii_case("localhost:"))
        .map(|_| &host_port[10..])
    {
        let port: u16 = port_str
            .parse()
            .with_context(|| format!("invalid listen port in '{raw}'"))?;
        return Ok(std::net::SocketAddr::from(([127, 0, 0, 1], port)));
    }
    let addr: std::net::SocketAddr = host_port
        .parse()
        .with_context(|| format!("invalid listen address '{raw}'"))?;
    anyhow::ensure!(
        addr.ip().is_loopback(),
        "listen_addr '{raw}' must be a loopback address (e.g. 127.0.0.1:47620 or [::1]:47620)"
    );
    Ok(addr)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HostConfig {
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub listen_addr: String,
    pub openvmm_bin: Option<PathBuf>,
    pub fastdesk_viewer_bin: Option<PathBuf>,
    pub openvmm_img_bin: Option<PathBuf>,
    pub uefi_firmware_path: Option<PathBuf>,
}

impl Default for HostConfig {
    fn default() -> Self {
        let data_dir = default_data_dir();
        let runtime_dir = default_runtime_dir(&data_dir);
        Self {
            data_dir,
            runtime_dir,
            listen_addr: DEFAULT_DAEMON_LISTEN.to_string(),
            openvmm_bin: None,
            fastdesk_viewer_bin: None,
            openvmm_img_bin: None,
            uefi_firmware_path: None,
        }
    }
}

pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = dirs::data_local_dir() {
        return dir.join("openvmms");
    }
    PathBuf::from(".openvmms")
}

pub fn default_runtime_dir(data_dir: &Path) -> PathBuf {
    #[cfg(unix)]
    if let Some(runtime) = dirs::runtime_dir() {
        return runtime.join("openvmms");
    }
    data_dir.join("run")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResolvedHostPaths {
    pub openvmm_bin: Option<PathBuf>,
    pub fastdesk_viewer_bin: Option<PathBuf>,
    pub openvmm_img_bin: Option<PathBuf>,
    pub uefi_firmware_path: Option<PathBuf>,
}

pub fn resolve_host_paths(cfg: &HostConfig) -> ResolvedHostPaths {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let cwd = std::env::current_dir().ok();

    let openvmm_name = if cfg!(windows) { "openvmm.exe" } else { "openvmm" };
    let viewer_name = if cfg!(windows) {
        "fastdesk-viewer.exe"
    } else {
        "fastdesk-viewer"
    };
    let img_name = if cfg!(windows) {
        "openvmm-img.exe"
    } else {
        "openvmm-img"
    };

    let openvmm_bin = resolve_binary_candidate(
        cfg.openvmm_bin.as_deref(),
        exe_dir.as_deref(),
        openvmm_name,
        &[
            &format!("openvmm/target/release/{openvmm_name}"),
            &format!("openvmm/target/debug/{openvmm_name}"),
            &format!("../openvmm/target/release/{openvmm_name}"),
            &format!("../openvmm/target/debug/{openvmm_name}"),
        ],
    );

    let fastdesk_viewer_bin = resolve_binary_candidate(
        cfg.fastdesk_viewer_bin.as_deref(),
        exe_dir.as_deref(),
        viewer_name,
        &[
            &format!("fastdesk-viewer/target/release/{viewer_name}"),
            &format!("fastdesk-viewer/target/debug/{viewer_name}"),
            &format!("../fastdesk-viewer/target/release/{viewer_name}"),
            &format!("../fastdesk-viewer/target/debug/{viewer_name}"),
        ],
    );

    let openvmm_img_bin = resolve_binary_candidate(
        cfg.openvmm_img_bin.as_deref(),
        exe_dir.as_deref(),
        img_name,
        &[
            &format!("openvmm/target/release/{img_name}"),
            &format!("openvmm/target/debug/{img_name}"),
            &format!("../openvmm/target/release/{img_name}"),
            &format!("../openvmm/target/debug/{img_name}"),
        ],
    );

    let uefi_firmware_path = resolve_file_candidate(
        cfg.uefi_firmware_path.as_deref(),
        exe_dir.as_deref(),
        cwd.as_deref(),
        std::env::var_os("X86_64_OPENVMM_UEFI_FIRMWARE")
            .map(PathBuf::from)
            .as_deref(),
        &[
            "firmware/MSVM.fd",
            "openvmm/.packages/hyperv.uefi.mscoreuefi.x64.RELEASE/MsvmX64/RELEASE_VS2022/FV/MSVM.fd",
            "../openvmm/.packages/hyperv.uefi.mscoreuefi.x64.RELEASE/MsvmX64/RELEASE_VS2022/FV/MSVM.fd",
        ],
    );

    ResolvedHostPaths {
        openvmm_bin,
        fastdesk_viewer_bin,
        openvmm_img_bin,
        uefi_firmware_path,
    }
}

fn resolve_binary_candidate(
    explicit: Option<&Path>,
    exe_dir: Option<&Path>,
    bin_name: &str,
    workspace_rel: &[&str],
) -> Option<PathBuf> {
    if let Some(p) = explicit
        && p.is_file()
    {
        return Some(p.to_path_buf());
    }
    if let Some(dir) = exe_dir {
        let candidate = dir.join(bin_name);
        if candidate.is_file() {
            return Some(candidate);
        }
        // Walk up a few directories in case running from target/debug, target/debug/deps, or target/release
        let mut cur = Some(dir);
        for _ in 0..5 {
            if let Some(parent) = cur {
                for rel in workspace_rel {
                    let c = parent.join(rel);
                    if c.is_file() {
                        return Some(c);
                    }
                }
                cur = parent.parent();
            }
        }
    }
    find_in_path(bin_name)
}

fn resolve_file_candidate(
    explicit: Option<&Path>,
    exe_dir: Option<&Path>,
    cwd: Option<&Path>,
    env_override: Option<&Path>,
    rel_paths: &[&str],
) -> Option<PathBuf> {
    if let Some(p) = explicit
        && p.is_file()
    {
        return Some(p.to_path_buf());
    }
    if let Some(p) = env_override
        && p.is_file()
    {
        return Some(p.to_path_buf());
    }
    if let Some(dir) = exe_dir {
        let mut cur = Some(dir);
        for _ in 0..4 {
            if let Some(parent) = cur {
                for rel in rel_paths {
                    let c = parent.join(rel);
                    if c.is_file() {
                        return Some(c);
                    }
                }
                cur = parent.parent();
            }
        }
    }
    if let Some(dir) = cwd {
        for rel in rel_paths {
            let c = dir.join(rel);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

fn find_in_path(bin_name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let full = dir.join(bin_name);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VmPreset {
    #[default]
    Windows11,
    LinuxDesktop,
    Custom,
}

impl std::fmt::Display for VmPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Windows11 => write!(f, "Windows 11"),
            Self::LinuxDesktop => write!(f, "Linux Desktop"),
            Self::Custom => write!(f, "Custom"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SecureBootTemplateConfig {
    None,
    #[default]
    MicrosoftWindows,
    MicrosoftUefiCa,
}

impl std::fmt::Display for SecureBootTemplateConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "None"),
            Self::MicrosoftWindows => write!(f, "Microsoft Windows"),
            Self::MicrosoftUefiCa => write!(f, "Microsoft UEFI CA"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiskFormat {
    #[default]
    Vhdx,
    Vhd,
    Raw,
    Iso,
}

impl DiskFormat {
    pub fn from_path(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("vhdx") => Self::Vhdx,
            Some("vhd") => Self::Vhd,
            Some("iso") => Self::Iso,
            _ => Self::Raw,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiskKind {
    #[default]
    HardDisk,
    Dvd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskAttachment {
    pub lun: u32,
    pub kind: DiskKind,
    pub path: PathBuf,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub format: DiskFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IpProto {
    #[default]
    Tcp,
    Udp,
}

impl std::fmt::Display for IpProto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp => write!(f, "tcp"),
            Self::Udp => write!(f, "udp"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortForward {
    #[serde(default)]
    pub protocol: IpProto,
    #[serde(default = "default_host_bind")]
    pub host_addr: String,
    pub host_port: u16,
    pub guest_port: u16,
}

fn default_host_bind() -> String {
    "127.0.0.1".to_string()
}

impl std::fmt::Display for PortForward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}:{}->{}",
            self.protocol, self.host_addr, self.host_port, self.guest_port
        )
    }
}

impl PortForward {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.host_addr
            .trim()
            .parse::<std::net::IpAddr>()
            .with_context(|| format!("invalid port forward host address '{}'", self.host_addr))?;
        if self.host_port == 0 {
            bail!("port forward host_port must be >= 1");
        }
        if self.guest_port == 0 {
            bail!("port forward guest_port must be >= 1");
        }
        Ok(())
    }
}

/// Parses specs like:
/// - `tcp:127.0.0.1:3389->3389`
/// - `tcp:8080->80`
/// - `udp:5000->5000`
/// - `8080->80` (defaults to tcp + 127.0.0.1)
pub fn parse_port_forward(spec: &str) -> anyhow::Result<PortForward> {
    let trimmed = spec.trim();
    let (left, right) = trimmed
        .split_once("->")
        .or_else(|| trimmed.split_once("-:"))
        .context("port forward must contain '->' (e.g. tcp:127.0.0.1:3389->3389)")?;

    let guest_port: u16 = right
        .trim()
        .trim_start_matches(':')
        .parse()
        .with_context(|| format!("invalid guest port in '{spec}'"))?;

    let (protocol, addr_part) = if left.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("tcp:")) {
        (IpProto::Tcp, &left[4..])
    } else if left.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("udp:")) {
        (IpProto::Udp, &left[4..])
    } else {
        (IpProto::Tcp, left)
    };

    let (host_addr, host_port_str) = if let Some(bracketed) = addr_part.strip_prefix('[') {
        let (ip, rest) = bracketed
            .split_once("]:")
            .context("invalid IPv6 host address in port forward")?;
        (ip.trim().to_string(), rest)
    } else if let Some((addr, port)) = addr_part.rsplit_once(':') {
        let h = if addr.trim().is_empty() {
            "127.0.0.1".to_string()
        } else {
            addr.trim().to_string()
        };
        (h, port)
    } else {
        ("127.0.0.1".to_string(), addr_part)
    };

    let host_port: u16 = host_port_str
        .trim()
        .parse()
        .with_context(|| format!("invalid host port in '{spec}'"))?;

    let pf = PortForward {
        protocol,
        host_addr,
        host_port,
        guest_port,
    };
    pf.validate()?;
    Ok(pf)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NicBackendConfig {
    Consomme {
        #[serde(default)]
        cidr: Option<String>,
        #[serde(default)]
        port_forwards: Vec<PortForward>,
        #[serde(default = "default_true")]
        internal_dns: bool,
        #[serde(default)]
        allow_host_local_access: bool,
    },
    VmSwitch {
        switch_id: String,
        #[serde(default)]
        switch_name: Option<String>,
    },
    Tap {
        name: String,
    },
}

fn default_true() -> bool {
    true
}

impl Default for NicBackendConfig {
    fn default() -> Self {
        Self::Consomme {
            cidr: None,
            port_forwards: Vec::new(),
            internal_dns: true,
            allow_host_local_access: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NicAttachment {
    pub id: Uuid,
    pub name: String,
    pub mac_address: String,
    pub backend: NicBackendConfig,
}

impl NicAttachment {
    pub fn default_consomme() -> Self {
        Self {
            id: Uuid::new_v4(),
            name: "Default NAT".to_string(),
            mac_address: generate_hyperv_mac(),
            backend: NicBackendConfig::default(),
        }
    }
}

/// Generates a random unicast MAC address in Microsoft's Hyper-V OUI (`00-15-5D-xx-xx-xx`).
pub fn generate_hyperv_mac() -> String {
    let bytes = Uuid::new_v4().into_bytes();
    format!("00-15-5D-{:02X}-{:02X}-{:02X}", bytes[0], bytes[1], bytes[2])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FastDeskTransport {
    #[default]
    Tcp,
    HvSock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FastDeskSettings {
    pub enabled: bool,
    pub vram_mb: u32,
    pub use_shm: bool,
    pub transport: FastDeskTransport,
    pub listen_addr: String,
    pub vsock_port: Option<u32>,
}

impl Default for FastDeskSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            vram_mb: 32,
            use_shm: true,
            transport: FastDeskTransport::Tcp,
            listen_addr: "127.0.0.1:0".to_string(),
            vsock_port: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmConfig {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub preset: VmPreset,
    pub vcpus: u32,
    pub memory_mb: u64,
    #[serde(default)]
    pub nested_virt: bool,
    #[serde(default)]
    pub uefi_firmware: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub secure_boot_enabled: bool,
    #[serde(default)]
    pub secure_boot_template: SecureBootTemplateConfig,
    #[serde(default)]
    pub vmgs_path: Option<PathBuf>,
    #[serde(default)]
    pub tpm_enabled: bool,
    #[serde(default)]
    pub fastdesk: FastDeskSettings,
    #[serde(default)]
    pub disks: Vec<DiskAttachment>,
    #[serde(default)]
    pub nics: Vec<NicAttachment>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl VmConfig {
    pub fn from_preset(name: impl Into<String>, preset: VmPreset, vm_dir: &Path) -> Self {
        let id = Uuid::new_v4();
        let now = Utc::now();
        let (vcpus, memory_mb, secure_boot_enabled, secure_boot_template, tpm_enabled) =
            match preset {
                VmPreset::Windows11 => (
                    4,
                    8192,
                    true,
                    SecureBootTemplateConfig::MicrosoftWindows,
                    true,
                ),
                VmPreset::LinuxDesktop => (
                    4,
                    4096,
                    false,
                    SecureBootTemplateConfig::MicrosoftUefiCa,
                    false,
                ),
                VmPreset::Custom => (2, 4096, false, SecureBootTemplateConfig::None, false),
            };
        Self {
            id,
            name: name.into(),
            description: String::new(),
            preset,
            vcpus,
            memory_mb,
            nested_virt: false,
            uefi_firmware: None,
            secure_boot_enabled,
            secure_boot_template,
            vmgs_path: Some(vm_dir.join("state.vmgs")),
            tpm_enabled,
            fastdesk: FastDeskSettings::default(),
            disks: Vec::new(),
            nics: vec![NicAttachment::default_consomme()],
            created_at: now,
            updated_at: now,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.name.trim().is_empty() {
            bail!("VM name cannot be empty");
        }
        if self.vcpus == 0 || self.vcpus > 256 {
            bail!("vcpus must be between 1 and 256");
        }
        if self.memory_mb < 128 || self.memory_mb > MAX_VM_MEMORY_MB {
            bail!("memory_mb must be between 128 and {MAX_VM_MEMORY_MB} MiB");
        }
        if self.fastdesk.enabled && (self.fastdesk.vram_mb < 8 || self.fastdesk.vram_mb > 256) {
            bail!("fastdesk.vram_mb must be between 8 and 256 MiB");
        }
        let mut seen_luns = std::collections::HashSet::new();
        for disk in &self.disks {
            if disk.path.as_os_str().is_empty() {
                bail!("disk path at SCSI LUN {} cannot be empty", disk.lun);
            }
            if (disk.format == DiskFormat::Iso || DiskFormat::from_path(&disk.path) == DiskFormat::Iso)
                && (disk.kind != DiskKind::Dvd || !disk.read_only)
            {
                bail!(
                    "ISO image '{}' at SCSI LUN {} must be attached as a read-only DVD drive",
                    disk.path.display(),
                    disk.lun
                );
            }
            if !seen_luns.insert(disk.lun) {
                bail!("duplicate SCSI LUN {}", disk.lun);
            }
        }
        for nic in &self.nics {
            if let NicBackendConfig::Consomme { port_forwards, .. } = &nic.backend {
                for pf in port_forwards {
                    pf.validate()?;
                }
            }
            #[cfg(target_os = "linux")]
            if let NicBackendConfig::Tap { name } = &nic.backend {
                validate_tap_sysfs_dir(name, &Path::new("/sys/class/net").join(name))?;
            }
        }
        Ok(())
    }

    pub fn next_free_lun(&self) -> u32 {
        (0u32..)
            .find(|lun| !self.disks.iter().any(|d| d.lun == *lun))
            .unwrap_or(0)
    }
}

/// Rejects Linux bridge interfaces passed where a persistent TAP device name is required.
pub fn validate_tap_sysfs_dir(name: &str, net_dir: &Path) -> anyhow::Result<()> {
    if net_dir.join("bridge").is_dir() {
        bail!(
            "interface '{name}' is a Linux bridge, not a TAP device; specify a persistent TAP interface"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VmStatus {
    #[default]
    Off,
    Starting,
    Running,
    Paused,
    Stopping,
    Error,
}

impl std::fmt::Display for VmStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => write!(f, "Off"),
            Self::Starting => write!(f, "Starting"),
            Self::Running => write!(f, "Running"),
            Self::Paused => write!(f, "Paused"),
            Self::Stopping => write!(f, "Stopping"),
            Self::Error => write!(f, "Error"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LiveVmProperties {
    pub state: String,
    pub halt_reason: Option<String>,
    pub working_set_bytes: Option<u64>,
    pub total_runtime_ns: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VmRuntimeState {
    pub status: VmStatus,
    pub pid: Option<u32>,
    pub started_at: Option<DateTime<Utc>>,
    pub fastdesk_endpoint: Option<String>,
    pub shm_path: Option<String>,
    pub rpc_socket_path: Option<PathBuf>,
    pub last_error: Option<String>,
    pub live_properties: Option<LiveVmProperties>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSummary {
    pub config: VmConfig,
    pub runtime: VmRuntimeState,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateVmPayload {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub preset: VmPreset,
    pub vcpus: Option<u32>,
    pub memory_mb: Option<u64>,
    pub nested_virt: Option<bool>,
    pub uefi_firmware: Option<PathBuf>,
    pub secure_boot_enabled: Option<bool>,
    pub secure_boot_template: Option<SecureBootTemplateConfig>,
    #[serde(default)]
    pub tpm_enabled: Option<bool>,
    pub fastdesk: Option<FastDeskSettings>,
    #[serde(default)]
    pub disks: Vec<DiskAttachment>,
    #[serde(default)]
    pub create_disk: Option<CreateDiskPayload>,
    pub iso_path: Option<PathBuf>,
    pub nics: Option<Vec<NicAttachment>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateVmPayload {
    pub name: Option<String>,
    pub description: Option<String>,
    pub vcpus: Option<u32>,
    pub memory_mb: Option<u64>,
    pub nested_virt: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_nullable_path"
    )]
    pub uefi_firmware: Option<Option<PathBuf>>,
    pub secure_boot_enabled: Option<bool>,
    pub secure_boot_template: Option<SecureBootTemplateConfig>,
    #[serde(default)]
    pub tpm_enabled: Option<bool>,
    pub fastdesk: Option<FastDeskSettings>,
    pub disks: Option<Vec<DiskAttachment>>,
    pub nics: Option<Vec<NicAttachment>>,
}

impl UpdateVmPayload {
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.description.is_none()
            && self.vcpus.is_none()
            && self.memory_mb.is_none()
            && self.nested_virt.is_none()
            && self.uefi_firmware.is_none()
            && self.secure_boot_enabled.is_none()
            && self.secure_boot_template.is_none()
            && self.tpm_enabled.is_none()
            && self.fastdesk.is_none()
            && self.disks.is_none()
            && self.nics.is_none()
    }
}

fn deserialize_nullable_path<'de, D>(deserializer: D) -> Result<Option<Option<PathBuf>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<PathBuf>::deserialize(deserializer)?;
    Ok(Some(opt.filter(|p| !p.as_os_str().is_empty())))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateDiskPayload {
    pub path: PathBuf,
    pub size_bytes: u64,
    #[serde(default)]
    pub format: DiskFormat,
    #[serde(default)]
    pub fixed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectDiskPayload {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachDiskPayload {
    pub path: PathBuf,
    pub lun: Option<u32>,
    #[serde(default)]
    pub read_only: bool,
    pub format: Option<DiskFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountIsoPayload {
    pub path: PathBuf,
    pub lun: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortForwardPayload {
    pub port: PortForward,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ShutdownVmPayload {
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub acpi: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskInfo {
    pub path: PathBuf,
    pub format: String,
    pub disk_type: String,
    pub virtual_size_bytes: u64,
    pub file_size_bytes: u64,
    pub block_size_bytes: Option<u32>,
    pub sector_size_bytes: Option<u32>,
    pub parent_path: Option<String>,
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualSwitchInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub cpu_count: usize,
    pub daemon_version: String,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub listen_addr: String,
    pub resolved_paths: ResolvedHostPaths,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DaemonEvent {
    VmUpdated { vm: VmSummary },
    VmDeleted { id: Uuid },
    HostConfigUpdated,
    DaemonStopping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorBody {
    pub error: String,
}

pub fn parse_byte_size(input: &str) -> anyhow::Result<u64> {
    let s = input.trim();
    if s.is_empty() {
        bail!("size cannot be empty");
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(s.len());
    let num: u64 = s[..split]
        .parse()
        .with_context(|| format!("invalid size number in '{input}'"))?;
    let unit = s[split..].trim().to_ascii_uppercase();
    let mult: u64 = match unit.as_str() {
        "" => bail!(
            "size '{input}' is missing a unit (specify B, K, M, G, or T, e.g. '{num}G' or '{num}M')"
        ),
        "B" => 1,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024 * 1024,
        "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
        "T" | "TB" | "TIB" => 1024 * 1024 * 1024 * 1024,
        _ => bail!("unknown size unit '{unit}' in '{input}'"),
    };
    num.checked_mul(mult)
        .context("size overflows u64")
}

/// Computes RFC 2104 HMAC-SHA256.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    const BLOCK_SIZE: usize = 64;
    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hashed = sha2::Sha256::digest(key);
        key_block[..32].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut o_key_pad = [0x5cu8; BLOCK_SIZE];
    let mut i_key_pad = [0x36u8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        o_key_pad[i] ^= key_block[i];
        i_key_pad[i] ^= key_block[i];
    }
    let mut inner = sha2::Sha256::new();
    inner.update(i_key_pad);
    inner.update(message);
    let inner_hash = inner.finalize();

    let mut outer = sha2::Sha256::new();
    outer.update(o_key_pad);
    outer.update(inner_hash);
    outer.finalize().into()
}

/// Computes the address-bound HMAC-SHA256 challenge-response digest returned by
/// `GET /api/v1/health?challenge=...` so clients can verify a local daemon knows
/// the bearer token and is bound to the exact dialed socket address.
pub fn compute_health_auth_proof(
    token: &str,
    bound_addr: std::net::SocketAddr,
    challenge: &str,
) -> String {
    let addr_str = bound_addr.to_string();
    let mut msg = Vec::with_capacity(20 + addr_str.len() + 1 + challenge.len());
    msg.extend_from_slice(b"openvmmsd-health-v2\0");
    msg.extend_from_slice(addr_str.as_bytes());
    msg.push(0);
    msg.extend_from_slice(challenge.as_bytes());
    let digest = hmac_sha256(token.as_bytes(), &msg);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub fn format_memory_mb(memory_mb: u64) -> String {
    let bytes_u128 = u128::from(memory_mb) * 1024 * 1024;
    if let Ok(bytes) = u64::try_from(bytes_u128) {
        format_bytes(bytes)
    } else {
        format!("{:.2} TiB", (memory_mb as f64) / (1024.0 * 1024.0))
    }
}

pub fn parse_memory_mb(input: &str) -> anyhow::Result<u64> {
    let trimmed = input.trim();
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        return trimmed
            .parse::<u64>()
            .context("invalid memory MiB integer");
    }
    let bytes = parse_byte_size(trimmed)?;
    Ok((bytes / (1024 * 1024)).max(1))
}

pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    const TIB: u64 = GIB * 1024;
    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_and_port_forward_parsing() {
        assert_eq!(parse_byte_size("64G").unwrap(), 64 * 1024 * 1024 * 1024);
        assert_eq!(parse_byte_size("512M").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_byte_size("4096B").unwrap(), 4096);
        assert!(
            parse_byte_size("64")
                .unwrap_err()
                .to_string()
                .contains("missing a unit")
        );
        assert_eq!(parse_memory_mb("8G").unwrap(), 8192);
        assert_eq!(parse_memory_mb("4096").unwrap(), 4096);

        // Known-answer RFC 4231 HMAC-SHA256 vector checks (Cases 1, 2, 3, and key > block size)
        // plus address-bound health auth proof verification.
        let to_hex = |bytes: &[u8]| -> String {
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        };
        assert_eq!(
            to_hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            to_hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            to_hex(&hmac_sha256(&[0xaa; 20], &[0xdd; 50])),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
        assert_eq!(
            to_hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );

        let addr1: std::net::SocketAddr = "127.0.0.1:47620".parse().unwrap();
        let addr2: std::net::SocketAddr = "127.0.0.1:47621".parse().unwrap();
        let p1 = compute_health_auth_proof("tok", addr1, "c1");
        let p2 = compute_health_auth_proof("tok", addr1, "c2");
        let p3 = compute_health_auth_proof("tok", addr2, "c1");
        assert_eq!(
            p1,
            "efcdf6cac3bf581bae72a74c3ab8aa71fec615fc38be554f965d1c1c7bb77a35"
        );
        assert_ne!(p1, p2);
        assert_ne!(p1, p3);

        let pf1 = parse_port_forward("tcp:127.0.0.1:3389->3389").unwrap();
        assert_eq!(pf1.protocol, IpProto::Tcp);
        assert_eq!(pf1.host_addr, "127.0.0.1");
        assert_eq!(pf1.host_port, 3389);
        assert_eq!(pf1.guest_port, 3389);

        let pf2 = parse_port_forward("udp:5000->5001").unwrap();
        assert_eq!(pf2.protocol, IpProto::Udp);
        assert_eq!(pf2.host_addr, "127.0.0.1");
        assert_eq!(pf2.host_port, 5000);
        assert_eq!(pf2.guest_port, 5001);

        let pf3 = parse_port_forward("UDP:53->53").unwrap();
        assert_eq!(pf3.protocol, IpProto::Udp);
        assert_eq!(pf3.host_addr, "127.0.0.1");
        assert_eq!(pf3.host_port, 53);
        assert_eq!(pf3.guest_port, 53);

        let pf4 = parse_port_forward("TCP:8080->80").unwrap();
        assert_eq!(pf4.protocol, IpProto::Tcp);
        assert_eq!(pf4.host_addr, "127.0.0.1");
        assert_eq!(pf4.host_port, 8080);
        assert_eq!(pf4.guest_port, 80);

        for bad in ["SCTP:53->53", "badhost:80->80", "0->80", "80->0"] {
            assert!(
                parse_port_forward(bad).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[test]
    fn preset_configs_are_valid() {
        let dir = Path::new("/tmp/test-vm");
        for preset in [VmPreset::Windows11, VmPreset::LinuxDesktop, VmPreset::Custom] {
            let cfg = VmConfig::from_preset("test-vm", preset, dir);
            cfg.validate().unwrap();
            assert_eq!(cfg.nics.len(), 1);
            assert!(cfg.nics[0].mac_address.starts_with("00-15-5D-"));
            assert_eq!(cfg.tpm_enabled, matches!(preset, VmPreset::Windows11));
        }
    }

    #[test]
    fn memory_bounds_and_loopback_listen_addr_validation() {
        let dir = Path::new("/tmp/test-vm");
        let mut cfg = VmConfig::from_preset("mem-test", VmPreset::Custom, dir);
        cfg.memory_mb = MAX_VM_MEMORY_MB;
        cfg.validate().unwrap();

        cfg.memory_mb = MAX_VM_MEMORY_MB + 1;
        assert!(cfg.validate().is_err());

        cfg.memory_mb = u64::MAX;
        assert!(cfg.validate().is_err());
        assert!(!format_memory_mb(u64::MAX).is_empty());

        assert_eq!(
            parse_loopback_listen_addr("localhost:47620").unwrap(),
            "127.0.0.1:47620".parse::<std::net::SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_loopback_listen_addr("http://localhost:47620/").unwrap(),
            "127.0.0.1:47620".parse::<std::net::SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_loopback_listen_addr("[::1]:47620").unwrap(),
            "[::1]:47620".parse::<std::net::SocketAddr>().unwrap()
        );
        assert!(parse_loopback_listen_addr("0.0.0.0:47620").is_err());
        assert!(parse_loopback_listen_addr("example.com:47620").is_err());
    }

    #[test]
    fn update_vm_payload_distinguishes_omitted_and_null_firmware() {
        let omitted: UpdateVmPayload = serde_json::from_str(r#"{"name":"vm1"}"#).unwrap();
        assert_eq!(omitted.uefi_firmware, None);

        let cleared: UpdateVmPayload = serde_json::from_str(r#"{"uefi_firmware":null}"#).unwrap();
        assert_eq!(cleared.uefi_firmware, Some(None));

        let empty: UpdateVmPayload = serde_json::from_str(r#"{"uefi_firmware":""}"#).unwrap();
        assert_eq!(empty.uefi_firmware, Some(None));

        let set: UpdateVmPayload =
            serde_json::from_str(r#"{"uefi_firmware":"/opt/MSVM.fd"}"#).unwrap();
        assert_eq!(set.uefi_firmware, Some(Some(PathBuf::from("/opt/MSVM.fd"))));
    }

    #[test]
    fn linux_bridge_rejected_as_tap_interface() {
        let dir = tempfile::tempdir().unwrap();
        let br_dir = dir.path().join("br0");
        std::fs::create_dir_all(br_dir.join("bridge")).unwrap();
        let err = validate_tap_sysfs_dir("br0", &br_dir).unwrap_err();
        assert!(err.to_string().contains("Linux bridge"));

        let tap_dir = dir.path().join("tap0");
        std::fs::create_dir_all(&tap_dir).unwrap();
        std::fs::write(tap_dir.join("tun_flags"), "0x1002\n").unwrap();
        validate_tap_sysfs_dir("tap0", &tap_dir).unwrap();
    }
}
