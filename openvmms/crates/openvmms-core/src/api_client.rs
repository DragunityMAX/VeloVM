use crate::models::ApiErrorBody;
use crate::models::AttachDiskPayload;
use crate::models::CreateDiskPayload;
use crate::models::CreateVmPayload;
use crate::models::DEFAULT_DAEMON_LISTEN;
use crate::models::DiskInfo;
use crate::models::HostConfig;
use crate::models::HostInfo;
use crate::models::InspectDiskPayload;
use crate::models::MountIsoPayload;
use crate::models::PortForward;
use crate::models::PortForwardPayload;
use crate::models::ShutdownVmPayload;
use crate::models::UpdateVmPayload;
use crate::models::VirtualSwitchInfo;
use crate::models::VmSummary;
use crate::models::default_data_dir;
use crate::store::RegistryStore;
use anyhow::Context;
use anyhow::bail;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ApiClient {
    base_url: String,
    token: String,
    client: reqwest::blocking::Client,
}

impl ApiClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> anyhow::Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            base_url,
            token: token.into(),
            client,
        })
    }

    pub fn from_data_dir(
        data_dir: Option<&Path>,
        daemon_url_override: Option<&str>,
    ) -> anyhow::Result<Self> {
        let dir = data_dir
            .map(resolve_client_path)
            .unwrap_or_else(default_data_dir);
        let store = RegistryStore::new(&dir)?;
        let cfg = store.load_or_init_host_config()?;
        let token = store.read_token()?;
        let base_url = daemon_url_override
            .map(|u| normalize_base_url(u))
            .unwrap_or_else(|| normalize_base_url(&cfg.listen_addr));
        Self::new(base_url, token)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn health(&self) -> anyhow::Result<()> {
        let resp = self
            .client
            .get(format!("{}/api/v1/health", self.base_url))
            .timeout(Duration::from_secs(2))
            .send()
            .context("connecting to openvmmsd")?;
        if !resp.status().is_success() {
            bail!("openvmmsd health returned {}", resp.status());
        }
        Ok(())
    }

    pub fn host_info(&self) -> anyhow::Result<HostInfo> {
        self.get("/api/v1/host/info")
    }

    pub fn host_config(&self) -> anyhow::Result<HostConfig> {
        self.get("/api/v1/host/config")
    }

    pub fn update_host_config(&self, cfg: &HostConfig) -> anyhow::Result<HostConfig> {
        let mut resolved = cfg.clone();
        if !resolved.data_dir.as_os_str().is_empty() {
            resolved.data_dir = resolve_client_path(&resolved.data_dir);
        }
        if !resolved.runtime_dir.as_os_str().is_empty() {
            resolved.runtime_dir = resolve_client_path(&resolved.runtime_dir);
        }
        resolved.openvmm_bin = resolved.openvmm_bin.as_deref().map(resolve_client_path);
        resolved.fastdesk_viewer_bin = resolved
            .fastdesk_viewer_bin
            .as_deref()
            .map(resolve_client_path);
        resolved.openvmm_img_bin = resolved.openvmm_img_bin.as_deref().map(resolve_client_path);
        resolved.uefi_firmware_path = resolved
            .uefi_firmware_path
            .as_deref()
            .map(resolve_client_path);
        self.put("/api/v1/host/config", &resolved)
    }

    pub fn list_switches(&self) -> anyhow::Result<Vec<VirtualSwitchInfo>> {
        self.get("/api/v1/host/switches")
    }

    pub fn list_vms(&self) -> anyhow::Result<Vec<VmSummary>> {
        self.get("/api/v1/vms")
    }

    pub fn get_vm(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        self.get(&format!("/api/v1/vms/{}", url_segment(id_or_name)))
    }

    pub fn create_vm(&self, req: &CreateVmPayload) -> anyhow::Result<VmSummary> {
        let mut resolved = req.clone();
        resolved.uefi_firmware = resolved.uefi_firmware.as_deref().map(resolve_client_path);
        resolved.iso_path = resolved.iso_path.as_deref().map(resolve_client_path);
        for d in &mut resolved.disks {
            d.path = resolve_client_path(&d.path);
        }
        if let Some(cd) = &mut resolved.create_disk
            && !cd.path.as_os_str().is_empty()
        {
            cd.path = resolve_client_path(&cd.path);
        }
        self.post("/api/v1/vms", &resolved)
    }

    pub fn update_vm(&self, id_or_name: &str, req: &UpdateVmPayload) -> anyhow::Result<VmSummary> {
        let mut resolved = req.clone();
        if let Some(fw_opt) = &resolved.uefi_firmware {
            resolved.uefi_firmware = Some(match fw_opt {
                Some(fw) if !fw.as_os_str().is_empty() => Some(resolve_client_path(fw)),
                _ => None,
            });
        }
        if let Some(disks) = &mut resolved.disks {
            for d in disks {
                d.path = resolve_client_path(&d.path);
            }
        }
        self.put(&format!("/api/v1/vms/{}", url_segment(id_or_name)), &resolved)
    }

    pub fn delete_vm(&self, id_or_name: &str, delete_disks: bool) -> anyhow::Result<()> {
        let suffix = if delete_disks {
            "?delete_disks=true"
        } else {
            ""
        };
        let url = format!("{}/api/v1/vms/{}{}", self.base_url, url_segment(id_or_name), suffix);
        let resp = self
            .client
            .delete(url)
            .bearer_auth(&self.token)
            .send()
            .context("DELETE VM request failed")?;
        decode_empty_response(resp)
    }

    pub fn start_vm(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        self.post_empty(&format!("/api/v1/vms/{}/start", url_segment(id_or_name)))
    }

    pub fn stop_vm(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        self.post_empty(&format!("/api/v1/vms/{}/stop", url_segment(id_or_name)))
    }

    pub fn shutdown_vm(
        &self,
        id_or_name: &str,
        req: &ShutdownVmPayload,
    ) -> anyhow::Result<VmSummary> {
        self.post(
            &format!("/api/v1/vms/{}/shutdown", url_segment(id_or_name)),
            req,
        )
    }

    pub fn pause_vm(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        self.post_empty(&format!("/api/v1/vms/{}/pause", url_segment(id_or_name)))
    }

    pub fn resume_vm(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        self.post_empty(&format!("/api/v1/vms/{}/resume", url_segment(id_or_name)))
    }

    pub fn reset_vm(&self, id_or_name: &str) -> anyhow::Result<VmSummary> {
        self.post_empty(&format!("/api/v1/vms/{}/reset", url_segment(id_or_name)))
    }

    pub fn attach_disk(
        &self,
        id_or_name: &str,
        req: &AttachDiskPayload,
    ) -> anyhow::Result<VmSummary> {
        let mut resolved = req.clone();
        resolved.path = resolve_client_path(&resolved.path);
        self.post(
            &format!("/api/v1/vms/{}/disks", url_segment(id_or_name)),
            &resolved,
        )
    }

    pub fn detach_disk(&self, id_or_name: &str, lun: u32) -> anyhow::Result<VmSummary> {
        self.delete_json(&format!(
            "/api/v1/vms/{}/disks/{}",
            url_segment(id_or_name),
            lun
        ))
    }

    pub fn mount_iso(&self, id_or_name: &str, req: &MountIsoPayload) -> anyhow::Result<VmSummary> {
        let mut resolved = req.clone();
        resolved.path = resolve_client_path(&resolved.path);
        self.post(
            &format!("/api/v1/vms/{}/iso", url_segment(id_or_name)),
            &resolved,
        )
    }

    pub fn eject_iso(&self, id_or_name: &str, lun: Option<u32>) -> anyhow::Result<VmSummary> {
        let path = match lun {
            Some(l) => format!("/api/v1/vms/{}/iso/{l}", url_segment(id_or_name)),
            None => format!("/api/v1/vms/{}/iso", url_segment(id_or_name)),
        };
        self.delete_json(&path)
    }

    pub fn add_port_forward(
        &self,
        id_or_name: &str,
        nic_id: Uuid,
        port: PortForward,
    ) -> anyhow::Result<VmSummary> {
        self.post(
            &format!(
                "/api/v1/vms/{}/nics/{nic_id}/ports",
                url_segment(id_or_name)
            ),
            &PortForwardPayload { port },
        )
    }

    pub fn remove_port_forward(
        &self,
        id_or_name: &str,
        nic_id: Uuid,
        port: PortForward,
    ) -> anyhow::Result<VmSummary> {
        let url = format!(
            "{}/api/v1/vms/{}/nics/{nic_id}/ports",
            self.base_url,
            url_segment(id_or_name)
        );
        let resp = self
            .client
            .delete(url)
            .bearer_auth(&self.token)
            .json(&PortForwardPayload { port })
            .send()
            .context("DELETE port forward request failed")?;
        decode_json_response(resp)
    }

    pub fn vm_logs(&self, id_or_name: &str, tail: usize) -> anyhow::Result<String> {
        #[derive(serde::Deserialize)]
        struct LogResponse {
            content: String,
        }
        let resp: LogResponse = self.get(&format!(
            "/api/v1/vms/{}/logs?tail={tail}",
            url_segment(id_or_name)
        ))?;
        Ok(resp.content)
    }

    pub fn create_disk(&self, req: &CreateDiskPayload) -> anyhow::Result<DiskInfo> {
        let mut resolved = req.clone();
        resolved.path = resolve_client_path(&resolved.path);
        self.post("/api/v1/disks/create", &resolved)
    }

    pub fn inspect_disk(&self, path: &Path) -> anyhow::Result<DiskInfo> {
        self.post(
            "/api/v1/disks/inspect",
            &InspectDiskPayload {
                path: resolve_client_path(path),
            },
        )
    }

    pub fn shutdown_daemon(&self) -> anyhow::Result<()> {
        let url = format!("{}/api/v1/daemon/shutdown", self.base_url);
        let resp = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .send()
            .context("sending daemon shutdown")?;
        decode_empty_response(resp)
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let resp = self
            .client
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("GET {path}"))?;
        decode_json_response(resp)
    }

    fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> anyhow::Result<T> {
        let resp = self
            .client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .with_context(|| format!("POST {path}"))?;
        decode_json_response(resp)
    }

    fn post_empty<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let resp = self
            .client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("POST {path}"))?;
        decode_json_response(resp)
    }

    fn put<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> anyhow::Result<T> {
        let resp = self
            .client
            .put(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .with_context(|| format!("PUT {path}"))?;
        decode_json_response(resp)
    }

    fn delete_json<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let resp = self
            .client
            .delete(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("DELETE {path}"))?;
        decode_json_response(resp)
    }
}

pub fn resolve_client_path(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return PathBuf::new();
    }
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    };
    normalize_client_path(&joined)
}

fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(s) = path.to_str()
            && let Some(stripped) = s.strip_prefix(r"\\?\")
        {
            return PathBuf::from(stripped);
        }
    }
    path
}

fn normalize_client_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Canonicalize the prefix before resolving `..` so symlinks expand to their target's parent rather than being collapsed lexically.
                if !out.as_os_str().is_empty()
                    && let Ok(canon) = std::fs::canonicalize(&out)
                {
                    out = strip_verbatim_prefix(canon);
                    out.pop();
                } else {
                    out.push(comp.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        path.to_path_buf()
    } else {
        out
    }
}

pub fn normalize_base_url(listen_or_url: &str) -> String {
    let trimmed = listen_or_url.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }
    let addr = if trimmed.is_empty() {
        DEFAULT_DAEMON_LISTEN
    } else {
        trimmed
    };
    format!("http://{addr}")
}

fn url_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn decode_json_response<T: DeserializeOwned>(resp: reqwest::blocking::Response) -> anyhow::Result<T> {
    let status = resp.status();
    if status.is_success() {
        resp.json::<T>().context("decoding daemon JSON response")
    } else {
        let text = resp.text().unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<ApiErrorBody>(&text) {
            bail!("daemon error ({status}): {}", err.error);
        }
        bail!("daemon error ({status}): {text}");
    }
}

fn decode_empty_response(resp: reqwest::blocking::Response) -> anyhow::Result<()> {
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let text = resp.text().unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<ApiErrorBody>(&text) {
            bail!("daemon error ({status}): {}", err.error);
        }
        bail!("daemon error ({status}): {text}");
    }
}

pub fn find_openvmmsd_binary() -> anyhow::Result<PathBuf> {
    let bin_name = if cfg!(windows) {
        "openvmmsd.exe"
    } else {
        "openvmmsd"
    };
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(bin_name);
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        for rel in [
            bin_name.to_string(),
            format!("openvmms/target/release/{bin_name}"),
            format!("openvmms/target/debug/{bin_name}"),
        ] {
            let p = cwd.join(rel);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let p = dir.join(bin_name);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    bail!("could not locate {bin_name} binary next to current executable or in PATH");
}

pub fn ensure_daemon_running(
    data_dir: Option<&Path>,
    daemon_url_override: Option<&str>,
) -> anyhow::Result<ApiClient> {
    if let Ok(client) = ApiClient::from_data_dir(data_dir, daemon_url_override)
        && client.health().is_ok()
    {
        return Ok(client);
    }

    let daemon_bin = find_openvmmsd_binary()?;
    let dir = data_dir
        .map(resolve_client_path)
        .unwrap_or_else(default_data_dir);
    let store = RegistryStore::new(&dir)?;
    let cfg = store.load_or_init_host_config()?;
    let _ = store.load_or_create_token()?;

    let mut cmd = Command::new(&daemon_bin);
    cmd.arg("--data-dir").arg(&dir);
    if let Some(url) = daemon_url_override {
        let listen = url
            .trim()
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_end_matches('/');
        cmd.arg("--listen").arg(listen);
    } else {
        cmd.arg("--listen").arg(&cfg.listen_addr);
    }
    let log_file_path = dir.join("openvmmsd.log");
    if let Ok(log_file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file_path)
    {
        let err_file = log_file.try_clone().ok();
        cmd.stdout(Stdio::from(log_file));
        if let Some(ef) = err_file {
            cmd.stderr(Stdio::from(ef));
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    cmd.stdin(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", daemon_bin.display()))?;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            bail!(
                "openvmmsd exited prematurely with {status} (see {})",
                log_file_path.display()
            );
        }
        if let Ok(client) = ApiClient::from_data_dir(Some(&dir), daemon_url_override)
            && client.health().is_ok()
        {
            return Ok(client);
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "timed out waiting for openvmmsd to become healthy (see {})",
                log_file_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn launch_fastdesk_viewer(
    viewer_bin: &Path,
    endpoint: &str,
    vm_name: &str,
    no_shm: bool,
) -> anyhow::Result<Child> {
    let mut cmd = Command::new(viewer_bin);
    cmd.arg("--connect")
        .arg(endpoint)
        .arg("--title")
        .arg(format!("{vm_name} - FastDesk"));
    if no_shm {
        cmd.arg("--no-shm");
    }
    cmd.spawn()
        .with_context(|| format!("launching fastdesk-viewer {}", viewer_bin.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_client_path_resolves_relative_and_preserves_empty() {
        assert_eq!(resolve_client_path(Path::new("")), PathBuf::new());
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let resolved = resolve_client_path(&sub.join("..").join("disk.img"));
        assert_eq!(
            resolved,
            strip_verbatim_prefix(std::fs::canonicalize(dir.path()).unwrap()).join("disk.img")
        );
        assert!(resolved.is_absolute());

        // Non-existent intermediate directory should keep `..` instead of collapsing it away.
        let nonexistent = resolve_client_path(&dir.path().join("missing").join("..").join("disk.img"));
        assert_eq!(
            nonexistent,
            dir.path().join("missing").join("..").join("disk.img")
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_client_path_preserves_symlink_parent_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("real_parent");
        let real_sub = real_parent.join("sub");
        std::fs::create_dir_all(&real_sub).unwrap();

        let wrong_disk = dir.path().join("disk.img");
        let right_disk = real_parent.join("disk.img");
        std::fs::write(&wrong_disk, b"WRONG").unwrap();
        std::fs::write(&right_disk, b"RIGHT").unwrap();

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real_sub, &link).unwrap();

        let resolved = resolve_client_path(&link.join("..").join("disk.img"));
        assert_eq!(resolved, std::fs::canonicalize(&right_disk).unwrap());
        assert_ne!(resolved, std::fs::canonicalize(&wrong_disk).unwrap());
    }
}
