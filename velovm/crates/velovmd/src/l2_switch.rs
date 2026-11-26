use anyhow::Context;
use anyhow::bail;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::debug;
use tracing::warn;
use velovm_core::models::VirtualSwitchInfo;
use velovm_core::models::validate_l2_switch_id;

/// Minimum Ethernet frame size (dst MAC + src MAC + EtherType).
pub const MIN_L2_FRAME_LEN: usize = 14;
/// Maximum Ethernet frame size (supports standard and jumbo frames).
pub const MAX_L2_FRAME_LEN: usize = 16 * 1024;
/// Default MAC learning table entry aging timeout (5 minutes, standard L2 bridge aging).
pub const DEFAULT_MAC_AGING: Duration = Duration::from_secs(300);
/// Per-port outbound queue depth before frames are dropped for backpressure.
pub const PORT_QUEUE_CAPACITY: usize = 256;

/// Architectural note on Optional Uplink / DHCP Integration:
///
/// `L2SwitchSpec::cidr` reserves the subnet metadata for an optional gateway/DHCP uplink port
/// attached as a virtual port on [`L2SwitchCore`]. Because `consomme` currently resides inside the
/// `openvmm` workspace (`openvmm/vm/devices/net/net_consomme/consomme`) rather than `velovm`,
/// VMs requiring outbound NAT + DHCP alongside private VM-to-VM L2 connectivity can attach both a
/// `NicBackendConfig::Consomme` NIC and a `NicBackendConfig::L2Switch` NIC, or a future in-daemon
/// gateway adapter can register a hub port via [`L2SwitchCore::add_port`] to bridge DHCP/NAT
/// frames directly onto the L2 hub.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct L2SwitchSpec {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cidr: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl L2SwitchSpec {
    pub fn to_switch_info(&self) -> VirtualSwitchInfo {
        VirtualSwitchInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: "l2".to_string(),
            description: "Cross-platform userspace L2 virtual switch".to_string(),
            switch_type: Some("l2".to_string()),
            managed: true,
            uplink_adapter: None,
            cidr: self.cidr.clone(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SwitchesFile {
    #[serde(default)]
    switches: Vec<L2SwitchSpec>,
}

pub fn l2_switch_endpoint(runtime_dir: &Path, switch_id: &str) -> String {
    #[cfg(unix)]
    {
        runtime_dir
            .join("switches")
            .join(format!("{switch_id}.sock"))
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        let _ = runtime_dir;
        format!(r"\\.\pipe\velovm-switch-{switch_id}")
    }
}

#[derive(Debug, Clone, Copy)]
struct MacEntry {
    port_id: u64,
    last_seen: Instant,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ForwardStats {
    pub unicast_forwarded: u64,
    pub flooded: u64,
    pub dropped_backpressure: u64,
    pub dropped_invalid: u64,
}

/// Pure MAC-learning L2 Ethernet switch forwarding table and per-port fan-out core.
pub struct L2SwitchCore {
    mac_aging: Duration,
    mac_table: HashMap<[u8; 6], MacEntry>,
    ports: HashMap<u64, mpsc::Sender<Arc<[u8]>>>,
    next_port_id: u64,
    stats: ForwardStats,
}

impl L2SwitchCore {
    pub fn new(mac_aging: Duration) -> Self {
        Self {
            mac_aging,
            mac_table: HashMap::new(),
            ports: HashMap::new(),
            next_port_id: 1,
            stats: ForwardStats::default(),
        }
    }

    pub fn add_port(&mut self, capacity: usize) -> (u64, mpsc::Receiver<Arc<[u8]>>) {
        let port_id = self.next_port_id;
        self.next_port_id += 1;
        let (tx, rx) = mpsc::channel(capacity.max(1));
        self.ports.insert(port_id, tx);
        (port_id, rx)
    }

    pub fn remove_port(&mut self, port_id: u64) {
        self.ports.remove(&port_id);
        self.mac_table.retain(|_, entry| entry.port_id != port_id);
    }

    pub fn port_count(&self) -> usize {
        self.ports.len()
    }

    pub fn learned_mac_count(&self) -> usize {
        self.mac_table.len()
    }

    pub fn lookup_mac(&self, mac: &[u8; 6], now: Instant) -> Option<u64> {
        self.mac_table.get(mac).and_then(|entry| {
            (now.saturating_duration_since(entry.last_seen) < self.mac_aging)
                .then_some(entry.port_id)
        })
    }

    pub fn stats(&self) -> ForwardStats {
        self.stats
    }

    /// Learns the source MAC, ages expired entries, and forwards/floods the frame.
    pub fn forward_frame(&mut self, src_port_id: u64, frame: &[u8], now: Instant) {
        if !(MIN_L2_FRAME_LEN..=MAX_L2_FRAME_LEN).contains(&frame.len()) {
            self.stats.dropped_invalid += 1;
            return;
        }

        let dst_mac: [u8; 6] = frame[0..6].try_into().unwrap();
        let src_mac: [u8; 6] = frame[6..12].try_into().unwrap();

        // Learn source MAC if it is a non-zero unicast address (I/G bit == 0).
        if (src_mac[0] & 0x01) == 0 && src_mac != [0; 6] {
            self.mac_table.insert(
                src_mac,
                MacEntry {
                    port_id: src_port_id,
                    last_seen: now,
                },
            );
        }

        // Age out stale MAC entries.
        let aging = self.mac_aging;
        self.mac_table
            .retain(|_, entry| now.saturating_duration_since(entry.last_seen) < aging);

        let payload: Arc<[u8]> = Arc::from(frame);
        let is_group_dst = (dst_mac[0] & 0x01) != 0;

        if !is_group_dst
            && let Some(target_port_id) = self.mac_table.get(&dst_mac).map(|e| e.port_id)
        {
            if target_port_id == src_port_id {
                return;
            }
            if let Some(tx) = self.ports.get(&target_port_id) {
                match tx.try_send(payload) {
                    Ok(()) => self.stats.unicast_forwarded += 1,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        self.stats.dropped_backpressure += 1;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        self.remove_port(target_port_id);
                    }
                }
                return;
            }
        }

        // Broadcast, multicast, or unknown unicast: flood to all other connected ports.
        let mut closed_ports = Vec::new();
        for (&port_id, tx) in &self.ports {
            if port_id == src_port_id {
                continue;
            }
            match tx.try_send(Arc::clone(&payload)) {
                Ok(()) => self.stats.flooded += 1,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.stats.dropped_backpressure += 1;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    closed_ports.push(port_id);
                }
            }
        }
        for port_id in closed_ports {
            self.remove_port(port_id);
        }
    }
}

struct ActiveSwitchHub {
    endpoint: String,
    shutdown_tx: watch::Sender<bool>,
    _core: Arc<Mutex<L2SwitchCore>>,
}

impl Drop for ActiveSwitchHub {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        #[cfg(unix)]
        {
            let _ = fs::remove_file(&self.endpoint);
        }
    }
}

/// Manages persisted L2 switch definitions and their live forwarding hub listeners.
pub struct L2SwitchManager {
    switches_path: PathBuf,
    specs: Mutex<HashMap<String, L2SwitchSpec>>,
    hubs: Mutex<HashMap<String, ActiveSwitchHub>>,
    active_conn_counter: Arc<AtomicU64>,
}

impl L2SwitchManager {
    pub fn new(data_dir: &Path) -> anyhow::Result<Self> {
        let switches_path = data_dir.join("switches.toml");
        let loaded = load_switches_file(&switches_path)?;
        let mut specs = HashMap::new();
        for spec in loaded {
            specs.insert(spec.id.clone(), spec);
        }
        Ok(Self {
            switches_path,
            specs: Mutex::new(specs),
            hubs: Mutex::new(HashMap::new()),
            active_conn_counter: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn list_switches(&self) -> Vec<VirtualSwitchInfo> {
        let guard = self.specs.lock().unwrap();
        let mut out: Vec<VirtualSwitchInfo> =
            guard.values().map(L2SwitchSpec::to_switch_info).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn contains_switch(&self, id_or_name: &str) -> bool {
        let target = id_or_name.trim();
        let guard = self.specs.lock().unwrap();
        guard
            .values()
            .any(|s| s.id == target || s.name.eq_ignore_ascii_case(target))
    }

    pub fn create_switch(
        &self,
        name: &str,
        cidr: Option<String>,
        runtime_dir: &Path,
    ) -> anyhow::Result<VirtualSwitchInfo> {
        let trimmed = name.trim();
        validate_l2_switch_id(trimmed)?;

        let spec = L2SwitchSpec {
            id: trimmed.to_string(),
            name: trimmed.to_string(),
            cidr,
            created_at: Utc::now(),
        };

        {
            let mut guard = self.specs.lock().unwrap();
            if guard.contains_key(&spec.id)
                || guard
                    .values()
                    .any(|s| s.name.eq_ignore_ascii_case(&spec.name))
            {
                bail!("userspace L2 switch '{}' already exists", spec.name);
            }
            guard.insert(spec.id.clone(), spec.clone());
            save_switches_file(&self.switches_path, &guard)?;
        }

        if let Err(err) = self.ensure_hub_running(&spec.id, runtime_dir) {
            let mut guard = self.specs.lock().unwrap();
            guard.remove(&spec.id);
            let _ = save_switches_file(&self.switches_path, &guard);
            return Err(err);
        }

        Ok(spec.to_switch_info())
    }

    pub fn delete_switch(&self, id_or_name: &str) -> anyhow::Result<bool> {
        let target = id_or_name.trim();
        let removed_id = {
            let mut guard = self.specs.lock().unwrap();
            let key = guard
                .values()
                .find(|s| s.id == target || s.name.eq_ignore_ascii_case(target))
                .map(|s| s.id.clone());
            let Some(id) = key else {
                return Ok(false);
            };
            guard.remove(&id);
            save_switches_file(&self.switches_path, &guard)?;
            id
        };

        self.hubs.lock().unwrap().remove(&removed_id);
        Ok(true)
    }

    pub fn ensure_switch_running(&self, runtime_dir: &Path, switch_id: &str) -> anyhow::Result<String> {
        let id = {
            let guard = self.specs.lock().unwrap();
            guard
                .values()
                .find(|s| s.id == switch_id || s.name.eq_ignore_ascii_case(switch_id))
                .map(|s| s.id.clone())
        }
        .with_context(|| format!("userspace L2 virtual switch '{switch_id}' does not exist"))?;
        self.ensure_hub_running(&id, runtime_dir)
    }

    pub fn start_all_persisted(&self, runtime_dir: &Path) {
        let ids: Vec<String> = self.specs.lock().unwrap().keys().cloned().collect();
        for id in ids {
            if let Err(err) = self.ensure_hub_running(&id, runtime_dir) {
                warn!(switch_id = %id, error = %err, "failed to start persisted L2 switch hub");
            }
        }
    }

    pub fn shutdown_all(&self) {
        self.hubs.lock().unwrap().clear();
    }

    fn ensure_hub_running(&self, switch_id: &str, runtime_dir: &Path) -> anyhow::Result<String> {
        let endpoint = l2_switch_endpoint(runtime_dir, switch_id);
        let mut hubs = self.hubs.lock().unwrap();
        if let Some(existing) = hubs.get(switch_id) {
            if existing.endpoint == endpoint && !*existing.shutdown_tx.borrow() {
                return Ok(existing.endpoint.clone());
            }
            hubs.remove(switch_id);
        }

        let handle = tokio::runtime::Handle::try_current()
            .context("L2 switch hub requires an active Tokio runtime")?;
        let hub = spawn_l2_hub(&handle, &endpoint, Arc::clone(&self.active_conn_counter))?;
        hubs.insert(switch_id.to_string(), hub);
        Ok(endpoint)
    }
}

fn load_switches_file(path: &Path) -> anyhow::Result<Vec<L2SwitchSpec>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: SwitchesFile =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    for s in &parsed.switches {
        validate_l2_switch_id(&s.id)?;
    }
    Ok(parsed.switches)
}

fn save_switches_file(path: &Path, map: &HashMap<String, L2SwitchSpec>) -> anyhow::Result<()> {
    let mut switches: Vec<L2SwitchSpec> = map.values().cloned().collect();
    switches.sort_by(|a, b| a.name.cmp(&b.name));
    let data = toml::to_string_pretty(&SwitchesFile { switches })
        .context("serializing switches.toml")?;
    velovm_core::store::atomic_write(path, data.as_bytes(), false)
        .with_context(|| format!("writing {}", path.display()))
}

fn spawn_l2_hub(
    rt: &tokio::runtime::Handle,
    endpoint: &str,
    conn_counter: Arc<AtomicU64>,
) -> anyhow::Result<ActiveSwitchHub> {
    let core = Arc::new(Mutex::new(L2SwitchCore::new(DEFAULT_MAC_AGING)));
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    #[cfg(unix)]
    {
        let sock_path = PathBuf::from(endpoint);
        if let Some(parent) = sock_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        let _ = fs::remove_file(&sock_path);
        let std_listener = std::os::unix::net::UnixListener::bind(&sock_path)
            .with_context(|| format!("binding L2 switch socket {}", sock_path.display()))?;
        std_listener
            .set_nonblocking(true)
            .context("configuring non-blocking UnixListener")?;
        let listener = {
            let _guard = rt.enter();
            tokio::net::UnixListener::from_std(std_listener)
                .context("wrapping L2 switch UnixListener")?
        };
        let core_clone = Arc::clone(&core);
        rt.spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accept_res = listener.accept() => {
                        match accept_res {
                            Ok((stream, _)) => {
                                let conn_id = conn_counter.fetch_add(1, Ordering::Relaxed);
                                debug!(conn_id, "L2 switch port connected");
                                let c = Arc::clone(&core_clone);
                                let rx = shutdown_rx.clone();
                                tokio::spawn(serve_l2_port_stream(stream, c, rx));
                            }
                            Err(err) => {
                                warn!(error = %err, "L2 switch UnixListener accept error");
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::PipeMode;
        use tokio::net::windows::named_pipe::ServerOptions;

        let pipe_name = endpoint.to_string();
        let mut server = {
            let _guard = rt.enter();
            ServerOptions::new()
                .first_pipe_instance(true)
                .pipe_mode(PipeMode::Byte)
                .in_buffer_size(65_536)
                .out_buffer_size(65_536)
                .create(&pipe_name)
                .with_context(|| format!("creating L2 switch named pipe {pipe_name}"))?
        };
        let core_clone = Arc::clone(&core);
        rt.spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    connect_res = server.connect() => {
                        match connect_res {
                            Ok(()) => {
                                let connected_pipe = server;
                                match ServerOptions::new()
                                    .pipe_mode(PipeMode::Byte)
                                    .in_buffer_size(65_536)
                                    .out_buffer_size(65_536)
                                    .create(&pipe_name)
                                {
                                    Ok(next_server) => server = next_server,
                                    Err(err) => {
                                        warn!(error = %err, pipe = %pipe_name, "failed to create next L2 named pipe instance");
                                        let conn_id = conn_counter.fetch_add(1, Ordering::Relaxed);
                                        debug!(conn_id, "L2 switch pipe port connected");
                                        let c = Arc::clone(&core_clone);
                                        let rx = shutdown_rx.clone();
                                        tokio::spawn(serve_l2_port_stream(connected_pipe, c, rx));
                                        break;
                                    }
                                }
                                let conn_id = conn_counter.fetch_add(1, Ordering::Relaxed);
                                debug!(conn_id, "L2 switch pipe port connected");
                                let c = Arc::clone(&core_clone);
                                let rx = shutdown_rx.clone();
                                tokio::spawn(serve_l2_port_stream(connected_pipe, c, rx));
                            }
                            Err(err) => {
                                warn!(error = %err, pipe = %pipe_name, "L2 switch named pipe connect error");
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    Ok(ActiveSwitchHub {
        endpoint: endpoint.to_string(),
        shutdown_tx,
        _core: core,
    })
}

async fn serve_l2_port_stream<S>(
    stream: S,
    core: Arc<Mutex<L2SwitchCore>>,
    mut shutdown_rx: watch::Receiver<bool>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (port_id, mut outbound_rx) = core.lock().unwrap().add_port(PORT_QUEUE_CAPACITY);
    let (mut reader, mut writer) = tokio::io::split(stream);

    let write_task = tokio::spawn(async move {
        while let Some(frame) = outbound_rx.recv().await {
            let Ok(len_u16) = u16::try_from(frame.len()) else {
                continue;
            };
            let hdr = len_u16.to_be_bytes();
            if writer.write_all(&hdr).await.is_err() || writer.write_all(&frame).await.is_err() {
                break;
            }
        }
    });

    let mut len_buf = [0u8; 2];
    let mut frame_buf = vec![0u8; MAX_L2_FRAME_LEN];
    loop {
        let read_hdr = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
            res = reader.read_exact(&mut len_buf) => res,
        };
        if read_hdr.is_err() {
            break;
        }
        let frame_len = u16::from_be_bytes(len_buf) as usize;
        if !(MIN_L2_FRAME_LEN..=MAX_L2_FRAME_LEN).contains(&frame_len) {
            break;
        }
        let read_payload = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
            res = reader.read_exact(&mut frame_buf[..frame_len]) => res,
        };
        if read_payload.is_err() {
            break;
        }
        core.lock()
            .unwrap()
            .forward_frame(port_id, &frame_buf[..frame_len], Instant::now());
    }

    core.lock().unwrap().remove_port(port_id);
    write_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(dst: [u8; 6], src: [u8; 6], payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(14 + payload.len());
        frame.extend_from_slice(&dst);
        frame.extend_from_slice(&src);
        frame.extend_from_slice(&[0x08, 0x00]); // IPv4 EtherType
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn mac_learning_unicast_forwarding_flooding_and_aging() {
        let mut core = L2SwitchCore::new(Duration::from_secs(10));
        let (p1, mut rx1) = core.add_port(16);
        let (p2, mut rx2) = core.add_port(16);
        let (_p3, mut rx3) = core.add_port(16);

        let mac_a = [0x00, 0x15, 0x5d, 0x00, 0x00, 0x01];
        let mac_b = [0x00, 0x15, 0x5d, 0x00, 0x00, 0x02];
        let bcast = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        let t0 = Instant::now();

        // 1. Port 1 broadcasts -> Port 2 and Port 3 receive, Port 1 does NOT; mac_a is learned on p1.
        let frame_bcast = make_frame(bcast, mac_a, b"arp-who-has");
        core.forward_frame(p1, &frame_bcast, t0);
        assert!(rx1.try_recv().is_err());
        assert_eq!(rx2.try_recv().unwrap().as_ref(), frame_bcast.as_slice());
        assert_eq!(rx3.try_recv().unwrap().as_ref(), frame_bcast.as_slice());
        assert_eq!(core.lookup_mac(&mac_a, t0), Some(p1));

        // 2. Port 2 sends unicast to mac_a -> ONLY Port 1 receives (Port 3 does NOT!).
        let frame_uni = make_frame(mac_a, mac_b, b"arp-reply");
        let t1 = t0 + Duration::from_secs(1);
        core.forward_frame(p2, &frame_uni, t1);
        assert_eq!(rx1.try_recv().unwrap().as_ref(), frame_uni.as_slice());
        assert!(rx2.try_recv().is_err());
        assert!(rx3.try_recv().is_err());
        assert_eq!(core.stats().unicast_forwarded, 1);

        // 3. After aging timeout expires, sending to mac_a floods again.
        let t_expired = t1 + Duration::from_secs(15);
        let frame_after_age = make_frame(mac_a, mac_b, b"stale");
        core.forward_frame(p2, &frame_after_age, t_expired);
        assert_eq!(rx1.try_recv().unwrap().as_ref(), frame_after_age.as_slice());
        assert_eq!(rx3.try_recv().unwrap().as_ref(), frame_after_age.as_slice());
        assert_eq!(core.lookup_mac(&mac_a, t_expired), None);

        // 4. Port disconnect removes port and purges its learned MACs.
        core.remove_port(p2);
        assert_eq!(core.port_count(), 2);
        assert_eq!(core.lookup_mac(&mac_b, t_expired), None);
    }

    #[test]
    fn backpressure_drops_overflow_on_slow_port_without_blocking_healthy_ports() {
        let mut core = L2SwitchCore::new(Duration::from_secs(60));
        let (p_src, _rx_src) = core.add_port(4);
        let (_p_slow, mut rx_slow) = core.add_port(2);
        let (_p_fast, mut rx_fast) = core.add_port(16);

        let mac_src = [0x00, 0x15, 0x5d, 0x01, 0x00, 0x01];
        let bcast = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        let t0 = Instant::now();

        for i in 0u8..5 {
            let frame = make_frame(bcast, mac_src, &[i]);
            core.forward_frame(p_src, &frame, t0);
        }

        // Slow port capped at 2 frames, remaining 3 dropped cleanly without blocking fast port.
        assert_eq!(core.stats().dropped_backpressure, 3);
        assert!(rx_slow.try_recv().is_ok());
        assert!(rx_slow.try_recv().is_ok());
        assert!(rx_slow.try_recv().is_err());

        for _ in 0..5 {
            assert!(rx_fast.try_recv().is_ok());
        }
        assert!(rx_fast.try_recv().is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_l2_hub_forwards_and_persists_switches() {
        let dir = tempfile::tempdir().unwrap();
        let rt_dir = dir.path().join("run");
        let mgr = L2SwitchManager::new(dir.path()).unwrap();
        let info = mgr
            .create_switch("lab-net", Some("10.42.0.0/24".into()), &rt_dir)
            .unwrap();
        assert_eq!(info.id, "lab-net");
        assert_eq!(info.kind, "l2");

        // Persisted switches.toml survives reload.
        let mgr_reloaded = L2SwitchManager::new(dir.path()).unwrap();
        assert_eq!(mgr_reloaded.list_switches().len(), 1);
        assert_eq!(
            mgr_reloaded.list_switches()[0].cidr.as_deref(),
            Some("10.42.0.0/24")
        );

        let sock_path = l2_switch_endpoint(&rt_dir, "lab-net");
        let mut c1 = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let mut c2 = tokio::net::UnixStream::connect(&sock_path).await.unwrap();

        let mac1 = [0x00, 0x15, 0x5d, 0x10, 0x00, 0x01];
        let mac2 = [0x00, 0x15, 0x5d, 0x10, 0x00, 0x02];
        let frame = make_frame(mac2, mac1, b"hello-vm2");
        let len_be = (frame.len() as u16).to_be_bytes();
        c1.write_all(&len_be).await.unwrap();
        c1.write_all(&frame).await.unwrap();

        let mut got_len = [0u8; 2];
        tokio::time::timeout(Duration::from_secs(2), c2.read_exact(&mut got_len))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(u16::from_be_bytes(got_len) as usize, frame.len());
        let mut got_frame = vec![0u8; frame.len()];
        c2.read_exact(&mut got_frame).await.unwrap();
        assert_eq!(got_frame, frame);

        assert!(mgr.delete_switch("lab-net").unwrap());
        assert!(mgr.list_switches().is_empty());
    }
}
