use super::*;
use velovm_core::models::VmPreset;
use velovm_core::models::VmStatus;

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

struct MockRequest {
    method: String,
    target: String,
    #[allow(dead_code)]
    body: Vec<u8>,
}

fn read_mock_request(stream: &mut std::net::TcpStream) -> Option<MockRequest> {
    use std::io::Read;

    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut raw = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let mut header_end = None;
    while header_end.is_none() {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => return None,
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
    let hdr_end = header_end?;
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
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let body = raw[hdr_end..raw.len().min(hdr_end + content_len)].to_vec();
    Some(MockRequest {
        method,
        target,
        body,
    })
}

fn mock_http_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn write_mock_json(stream: &mut std::net::TcpStream, status: &str, body: &str) {
    use std::io::Write;
    let resp = mock_http_response(status, body);
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn write_sse_ok_headers(stream: &mut std::net::TcpStream) -> bool {
    use std::io::Write;
    let ok = stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        )
        .is_ok();
    let _ = stream.flush();
    ok
}

fn stream_sse_keepalives(
    stream: &mut std::net::TcpStream,
    stop: &AtomicBool,
    interval: Duration,
) {
    use std::io::Write;
    while !stop.load(Ordering::Acquire) {
        if stream.write_all(b": keepalive\n\n").is_err() {
            break;
        }
        let _ = stream.flush();
        thread::sleep(interval);
    }
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
        let proof = velovm_core::models::compute_health_auth_proof(token, addr, challenge);
        Some((
            "200 OK",
            format!(r#"{{"status":"ok","service":"velovmd","auth_proof":"{proof}"}}"#),
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

struct TestMockServer {
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TestMockServer {
    fn spawn_raw<F>(on_accept: F) -> Self
    where
        F: Fn(std::net::TcpStream, std::net::SocketAddr, Arc<AtomicBool>) + Send + Sync + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let on_accept = Arc::new(on_accept);
        let thread = thread::spawn(move || {
            let mut handlers = Vec::new();
            while !stop_srv.load(Ordering::Acquire) {
                match accept_blocking_stream(&listener) {
                    Ok((stream, _)) => {
                        let stop_conn = Arc::clone(&stop_srv);
                        let on_accept = Arc::clone(&on_accept);
                        handlers.push(thread::spawn(move || {
                            on_accept(stream, addr, stop_conn);
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
            for h in handlers {
                let _ = h.join();
            }
        });
        Self {
            addr,
            stop,
            thread: Some(thread),
        }
    }

    fn spawn<F>(on_request: F) -> Self
    where
        F: Fn(std::net::TcpStream, MockRequest, std::net::SocketAddr, &AtomicBool)
            + Send
            + Sync
            + 'static,
    {
        let on_request = Arc::new(on_request);
        Self::spawn_raw(move |mut stream, addr, stop| {
            let Some(req) = read_mock_request(&mut stream) else {
                return;
            };
            on_request(stream, req, addr, &stop);
        })
    }
}

impl Drop for TestMockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn backend_handle_follows_external_daemon_port_change_for_requests_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let daemon = rt
        .block_on(velovmd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
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
        dialog_id: Uuid::new_v4(),
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
        .block_on(velovmd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
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
        .block_on(velovmd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
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
        .block_on(velovmd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
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
    let squatter_hit = Arc::new(AtomicBool::new(false));
    let squatter_hit_srv = Arc::clone(&squatter_hit);
    let squatter = TestMockServer::spawn_raw(move |_stream, _addr, _stop| {
        squatter_hit_srv.store(true, Ordering::SeqCst);
    });
    let squatter_addr = squatter.addr;

    // Pre-seed config.toml with the occupied squatter address before starting the ephemeral daemon.
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let mut init_cfg = store.load_or_init_host_config().unwrap();
    init_cfg.listen_addr = squatter_addr.to_string();
    store.save_host_config(&init_cfg).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let daemon = rt
        .block_on(velovmd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
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
    let host_dialog_id = Uuid::new_v4();
    backend.send(GuiRequest::SaveHostConfig {
        dialog_id: host_dialog_id,
        patch,
    });

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
                    kind: DialogKind::HostSettings(did),
                    error: None,
                } if did == host_dialog_id => {
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

    drop(squatter);
    rt.block_on(daemon.shutdown());
}

#[test]
fn stop_vm_does_not_queue_behind_slow_start_vm() {
    let token = "concurrent-test-token";
    let vm_id = Uuid::new_v4();
    let mut vm_cfg = velovm_core::models::VmConfig::from_preset(
        "Concurrent-UI-VM",
        VmPreset::Custom,
        Path::new("/tmp/vm"),
    );
    vm_cfg.id = vm_id;

    let start_entered = Arc::new(AtomicBool::new(false));
    let release_start = Arc::new(AtomicBool::new(false));

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    velovm_core::store::atomic_write(&store.token_path(), token.as_bytes(), true).unwrap();

    let srv = {
        let start_entered = Arc::clone(&start_entered);
        let release_start = Arc::clone(&release_start);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let method = req.method.as_str();
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                if write_sse_ok_headers(&mut stream) {
                    while !stop.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(25));
                    }
                }
                return;
            }
            let (status, body) = if target == "/api/v1/vms" {
                let summary = VmSummary {
                    config: vm_cfg.clone(),
                    runtime: Default::default(),
                };
                ("200 OK", serde_json::to_string(&vec![summary]).unwrap())
            } else if method == "POST" && target.ends_with("/start") {
                start_entered.store(true, Ordering::Release);
                while !release_start.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(20));
                }
                let mut rt = velovm_core::models::VmRuntimeState::default();
                rt.status = VmStatus::Off;
                let summary = VmSummary {
                    config: vm_cfg.clone(),
                    runtime: rt,
                };
                ("200 OK", serde_json::to_string(&summary).unwrap())
            } else if method == "POST" && target.ends_with("/stop") {
                let mut rt = velovm_core::models::VmRuntimeState::default();
                rt.status = VmStatus::Off;
                let summary = VmSummary {
                    config: vm_cfg.clone(),
                    runtime: rt,
                };
                ("200 OK", serde_json::to_string(&summary).unwrap())
            } else {
                mock_common_endpoint(target, token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", r#"{"error":"not found"}"#.into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };
    let addr = srv.addr;
    store.write_runtime_listen_addr(&addr.to_string()).unwrap();

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
    drop(backend);
    drop(srv);
}

#[test]
fn non_idempotent_create_and_delete_are_not_retried_after_port_change() {
    use std::sync::atomic::AtomicUsize;

    fn spawn_mock_endpoint(
        token: &'static str,
        store_dir: PathBuf,
        vm_summary: VmSummary,
        post_count: Arc<AtomicUsize>,
        delete_count: Arc<AtomicUsize>,
        switch_on_post: Arc<std::sync::Mutex<Option<std::net::SocketAddr>>>,
        switch_on_delete: Arc<std::sync::Mutex<Option<std::net::SocketAddr>>>,
    ) -> TestMockServer {
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let method = req.method.as_str();
            let target = req.target.as_str();

            if target == "/api/v1/events" {
                if write_sse_ok_headers(&mut stream) {
                    stream_sse_keepalives(&mut stream, stop, Duration::from_millis(25));
                }
                return;
            }

            if method == "POST" && target == "/api/v1/vms" {
                post_count.fetch_add(1, Ordering::SeqCst);
                if let Some(next_addr) = *switch_on_post.lock().unwrap() {
                    let store = velovm_core::store::RegistryStore::new(&store_dir).unwrap();
                    store
                        .write_runtime_listen_addr(&next_addr.to_string())
                        .unwrap();
                    return;
                }
            } else if method == "DELETE" && target.starts_with("/api/v1/vms/") {
                delete_count.fetch_add(1, Ordering::SeqCst);
                if let Some(next_addr) = *switch_on_delete.lock().unwrap() {
                    let store = velovm_core::store::RegistryStore::new(&store_dir).unwrap();
                    store
                        .write_runtime_listen_addr(&next_addr.to_string())
                        .unwrap();
                    return;
                }
            }

            let (status, body) = if method == "GET" && target == "/api/v1/vms" {
                (
                    "200 OK",
                    serde_json::to_string(&vec![vm_summary.clone()]).unwrap(),
                )
            } else {
                mock_common_endpoint(target, token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    }

    let token = "no-retry-token";
    let vm_id = Uuid::new_v4();
    let mut vm_cfg = velovm_core::models::VmConfig::from_preset(
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
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    velovm_core::store::atomic_write(&store.token_path(), token.as_bytes(), true).unwrap();

    let p1_posts = Arc::new(AtomicUsize::new(0));
    let p1_deletes = Arc::new(AtomicUsize::new(0));
    let p2_posts = Arc::new(AtomicUsize::new(0));
    let p2_deletes = Arc::new(AtomicUsize::new(0));
    let p3_posts = Arc::new(AtomicUsize::new(0));
    let p3_deletes = Arc::new(AtomicUsize::new(0));

    let p1_switch_post = Arc::new(std::sync::Mutex::new(None));
    let p2_switch_delete = Arc::new(std::sync::Mutex::new(None));

    let srv3 = spawn_mock_endpoint(
        token,
        dir.path().to_path_buf(),
        vm_summary.clone(),
        Arc::clone(&p3_posts),
        Arc::clone(&p3_deletes),
        Arc::new(std::sync::Mutex::new(None)),
        Arc::new(std::sync::Mutex::new(None)),
    );
    let addr3 = srv3.addr;
    *p2_switch_delete.lock().unwrap() = Some(addr3);

    let srv2 = spawn_mock_endpoint(
        token,
        dir.path().to_path_buf(),
        vm_summary.clone(),
        Arc::clone(&p2_posts),
        Arc::clone(&p2_deletes),
        Arc::new(std::sync::Mutex::new(None)),
        p2_switch_delete,
    );
    let addr2 = srv2.addr;
    *p1_switch_post.lock().unwrap() = Some(addr2);

    let srv1 = spawn_mock_endpoint(
        token,
        dir.path().to_path_buf(),
        vm_summary,
        Arc::clone(&p1_posts),
        Arc::clone(&p1_deletes),
        p1_switch_post,
        Arc::new(std::sync::Mutex::new(None)),
    );
    let addr1 = srv1.addr;
    store.write_runtime_listen_addr(&addr1.to_string()).unwrap();

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
    let del_dialog_id = Uuid::new_v4();
    backend.send(GuiRequest::DeleteVm {
        dialog_id: del_dialog_id,
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
                    kind: DialogKind::DeleteVm(did),
                    error: Some(_),
                } if did == del_dialog_id => {
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
    drop(srv1);
    drop(srv2);
    drop(srv3);
}

#[test]
fn sse_read_timeout_and_resync_event_reconcile_state() {
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let sse_count = Arc::new(AtomicUsize::new(0));
    let vms_stage = Arc::new(AtomicUsize::new(0));
    let send_resync = Arc::new(AtomicBool::new(false));
    let health_wedged = Arc::new(AtomicBool::new(false));

    let vm_id = Uuid::new_v4();
    let mut base_cfg = velovm_core::models::VmConfig::from_preset(
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
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                let idx = sse_count.fetch_add(1, Ordering::SeqCst) + 1;
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
                // Match daemon subscribe behavior: emit an initial leading Resync frame.
                let leading_json = serde_json::to_string(&DaemonEvent::Resync).unwrap();
                let _ = stream.write_all(format!("data: {leading_json}\n\n").as_bytes());
                let _ = stream.flush();
                if idx == 1 {
                    // Keep SSE stream alive with keep-alives until stage 1 Resync is requested.
                    while !stop.load(Ordering::Acquire) && !send_resync.load(Ordering::Acquire) {
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

            let (status, body) = if target.starts_with("/api/v1/health?")
                && health_wedged.load(Ordering::Acquire)
            {
                ("503 Service Unavailable", r#"{"status":"wedged"}"#.to_string())
            } else if target == "/api/v1/vms" {
                let stage = vms_stage.load(Ordering::Acquire) as u64;
                let vm_summary = VmSummary {
                    config: base_cfg.clone(),
                    runtime: velovm_core::models::VmRuntimeState {
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
                mock_common_endpoint(target, &token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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
    drop(server);
}

#[test]
fn first_sse_connect_refreshes_missed_vm_changes() {
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let sse_connected = Arc::new(AtomicBool::new(false));
    let vms_reqs = Arc::new(AtomicUsize::new(0));

    let mut vm_cfg = velovm_core::models::VmConfig::from_preset(
        "GapWindow-VM",
        VmPreset::Custom,
        &dir.path().join("vm"),
    );
    vm_cfg.id = Uuid::new_v4();

    let server = {
        let token = token.clone();
        let sse_connected = Arc::clone(&sse_connected);
        let vms_reqs = Arc::clone(&vms_reqs);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                // Record SSE arrival; send no events yet so only initial connect sync can surface the VM.
                sse_connected.store(true, Ordering::Release);
                if write_sse_ok_headers(&mut stream) {
                    stream_sse_keepalives(&mut stream, stop, Duration::from_millis(25));
                }
                return;
            }
            let (status, body) = if target == "/api/v1/vms" {
                let req_idx = vms_reqs.fetch_add(1, Ordering::SeqCst);
                if req_idx == 0 && !sse_connected.load(Ordering::Acquire) {
                    ("200 OK", "[]".to_string())
                } else {
                    let vm_summary = VmSummary {
                        config: vm_cfg.clone(),
                        runtime: velovm_core::models::VmRuntimeState {
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
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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
    drop(server);
}

#[test]
fn sse_reconnect_retries_snapshot_after_reload_failure() {
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let sse_count = Arc::new(AtomicUsize::new(0));
    let mut recovered_cfg = velovm_core::models::VmConfig::from_preset(
        "Recovered-VM",
        VmPreset::Custom,
        &dir.path().join("vm"),
    );
    recovered_cfg.id = Uuid::new_v4();

    let server = {
        let token = token.clone();
        let sse_count = Arc::clone(&sse_count);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                let idx = sse_count.fetch_add(1, Ordering::SeqCst) + 1;
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
                // Match daemon subscribe behavior: emit an initial leading Resync frame.
                let leading_json = serde_json::to_string(&DaemonEvent::Resync).unwrap();
                let _ = stream.write_all(format!("data: {leading_json}\n\n").as_bytes());
                let _ = stream.flush();
                if idx == 1 {
                    // Force quick EOF/reconnect after initial sync.
                    thread::sleep(Duration::from_millis(50));
                } else {
                    stream_sse_keepalives(&mut stream, stop, Duration::from_millis(25));
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
                        config: recovered_cfg.clone(),
                        runtime: velovm_core::models::VmRuntimeState {
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
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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
    drop(server);
}

#[test]
fn host_config_patch_preserves_concurrent_cli_changes() {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let daemon = rt
        .block_on(velovmd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
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
    backend.send(GuiRequest::SaveHostConfig {
        dialog_id: Uuid::new_v4(),
        patch,
    });

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
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let vms_reqs = Arc::new(AtomicUsize::new(0));
    let send_second_resync = Arc::new(AtomicBool::new(false));

    let server = {
        let token = token.clone();
        let vms_reqs = Arc::clone(&vms_reqs);
        let send_second_resync = Arc::clone(&send_second_resync);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n: keep-alive\n\ndata: {\"event\":\"resync\"}\n\n",
                );
                let _ = stream.flush();
                let mut sent_second = false;
                while !stop.load(Ordering::Acquire) {
                    if !sent_second && send_second_resync.load(Ordering::Acquire) {
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
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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
    drop(server);
}

#[test]
fn midstream_resync_and_host_config_snapshot_failures_reconnect_and_recover() {
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let sse_count = Arc::new(AtomicUsize::new(0));
    let fail_vms = Arc::new(AtomicBool::new(false));
    let send_resync = Arc::new(AtomicBool::new(false));
    let send_host_cfg = Arc::new(AtomicBool::new(false));
    let mut recovered_cfg = velovm_core::models::VmConfig::from_preset(
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
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                let idx = sse_count.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\ndata: {\"event\":\"resync\"}\n\n",
                );
                let _ = stream.flush();
                let mut sent_mid = false;
                while !stop.load(Ordering::Acquire) {
                    if idx == 1 && !sent_mid && send_resync.load(Ordering::Acquire) {
                        sent_mid = true;
                        if stream
                            .write_all(b"data: {\"event\":\"resync\"}\n\n")
                            .is_err()
                        {
                            break;
                        }
                        let _ = stream.flush();
                    } else if idx >= 2 && !sent_mid && send_host_cfg.swap(false, Ordering::AcqRel) {
                        sent_mid = true;
                        if stream
                            .write_all(b"data: {\"event\":\"host_config_updated\"}\n\n")
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
                        config: recovered_cfg.clone(),
                        runtime: velovm_core::models::VmRuntimeState {
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
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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
    drop(server);
}

#[test]
fn duplicate_storage_op_emits_finished_for_rejected_op_id_and_refresh_reconnect_resets_disconnect_tracking() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let health_fail = Arc::new(AtomicBool::new(false));
    let sse_fail = Arc::new(AtomicBool::new(false));
    let vm_id = Uuid::new_v4();
    let mut vm_cfg = velovm_core::models::VmConfig::from_preset(
        "OpId-VM",
        VmPreset::Custom,
        &dir.path().join("vm"),
    );
    vm_cfg.id = vm_id;

    let server = {
        let token = token.clone();
        let health_fail = Arc::clone(&health_fail);
        let sse_fail = Arc::clone(&sse_fail);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let method = req.method.as_str();
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                if sse_fail.load(Ordering::Acquire) {
                    write_mock_json(&mut stream, "503 Service Unavailable", "{}");
                    return;
                }
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
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
                write_mock_json(&mut stream, "503 Service Unavailable", "{}");
                return;
            }
            if method == "POST" && target.ends_with("/disks") {
                thread::sleep(Duration::from_millis(250));
                let summary = VmSummary {
                    config: vm_cfg.clone(),
                    runtime: Default::default(),
                };
                write_mock_json(
                    &mut stream,
                    "200 OK",
                    &serde_json::to_string(&summary).unwrap(),
                );
                return;
            }
            let (status, body) = if target == "/api/v1/vms" {
                ("200 OK", "[]".to_string())
            } else {
                mock_common_endpoint(target, &token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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
    drop(server);
}

#[test]
fn storage_op_retry_succeeds_after_daemon_instance_switch_while_prior_op_blocked() {
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let daemon_a = Uuid::new_v4();
    let daemon_b = Uuid::new_v4();
    let current_instance_id = Arc::new(std::sync::Mutex::new(daemon_a));
    let attach_count = Arc::new(AtomicUsize::new(0));
    let first_attach_started = Arc::new(AtomicBool::new(false));
    let unblock_first_attach = Arc::new(AtomicBool::new(false));

    let vm_id = Uuid::new_v4();
    let mut vm_cfg = velovm_core::models::VmConfig::from_preset(
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
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let method = req.method.as_str();
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                if write_sse_ok_headers(&mut stream) {
                    stream_sse_keepalives(&mut stream, stop, Duration::from_millis(25));
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
                    config: vm_cfg.clone(),
                    runtime: Default::default(),
                };
                write_mock_json(
                    &mut stream,
                    "200 OK",
                    &serde_json::to_string(&summary).unwrap(),
                );
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
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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
    drop(server);
}

#[test]
fn sse_backs_off_and_reports_error_when_events_fail_on_healthy_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let events_times = Arc::new(std::sync::Mutex::new(Vec::<std::time::Instant>::new()));

    let server = {
        let token = token.clone();
        let events_times = Arc::clone(&events_times);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, _stop| {
            let target = req.target.as_str();
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
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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
    drop(server);
}

#[test]
fn save_host_config_succeeds_when_followup_host_info_fails() {
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let host_info_reqs = Arc::new(AtomicUsize::new(0));
    let fail_followup_info = Arc::new(AtomicBool::new(false));
    let patch_applied = Arc::new(AtomicBool::new(false));
    let new_bin = dir.path().join("updated-openvmm");

    let server = {
        let token = token.clone();
        let host_info_reqs = Arc::clone(&host_info_reqs);
        let fail_followup_info = Arc::clone(&fail_followup_info);
        let patch_applied = Arc::clone(&patch_applied);
        let store_dir = dir.path().to_path_buf();
        let new_bin = new_bin.clone();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let method = req.method.as_str();
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                if write_sse_ok_headers(&mut stream) {
                    stream_sse_keepalives(&mut stream, stop, Duration::from_millis(25));
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
                    mock_common_endpoint(target, &token, addr, &store_dir).unwrap()
                }
            } else if method == "PATCH" && target == "/api/v1/host/config" {
                patch_applied.store(true, Ordering::Release);
                let mut cfg = HostConfig::default();
                cfg.openvmm_bin = Some(new_bin.clone());
                ("200 OK", serde_json::to_string(&cfg).unwrap())
            } else if method == "GET"
                && target == "/api/v1/host/config"
                && patch_applied.load(Ordering::Acquire)
            {
                let mut cfg = HostConfig::default();
                cfg.openvmm_bin = Some(new_bin.clone());
                ("200 OK", serde_json::to_string(&cfg).unwrap())
            } else if target == "/api/v1/vms" {
                ("200 OK", "[]".to_string())
            } else {
                mock_common_endpoint(target, &token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
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

    fail_followup_info.store(true, Ordering::Release);
    let before_reqs = host_info_reqs.load(Ordering::Acquire);
    let patch = HostConfigPatch {
        openvmm_bin: Some(Some(new_bin.clone())),
        ..Default::default()
    };
    let host_dialog_id = Uuid::new_v4();
    backend.send(GuiRequest::SaveHostConfig {
        dialog_id: host_dialog_id,
        patch,
    });

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
                    kind: DialogKind::HostSettings(did),
                    error,
                } if did == host_dialog_id => {
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
    drop(server);
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
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let events_times = Arc::new(std::sync::Mutex::new(Vec::<std::time::Instant>::new()));
    let sse_attempts = Arc::new(AtomicUsize::new(0));
    let send_stopping_1 = Arc::new(AtomicBool::new(false));
    let send_stopping_2 = Arc::new(AtomicBool::new(false));
    let stopping_1_at = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
    let stopping_2_at = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));

    let vm_4_cfg = velovm_core::models::VmConfig::from_preset(
        "Attempt4",
        VmPreset::Custom,
        &dir.path().join("vm4"),
    );
    let vm_5_cfg = velovm_core::models::VmConfig::from_preset(
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
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                let idx = sse_attempts.fetch_add(1, Ordering::SeqCst);
                events_times.lock().unwrap().push(std::time::Instant::now());
                if idx < 4 {
                    write_mock_json(
                        &mut stream,
                        "500 Internal Server Error",
                        r#"{"error":"transient events error"}"#,
                    );
                    return;
                }
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
                let (flag, sent_at) = if idx == 4 {
                    (&send_stopping_1, &stopping_1_at)
                } else if idx == 5 {
                    (&send_stopping_2, &stopping_2_at)
                } else {
                    stream_sse_keepalives(&mut stream, stop, Duration::from_millis(20));
                    return;
                };
                while !stop.load(Ordering::Acquire) {
                    if flag.load(Ordering::Acquire) {
                        *sent_at.lock().unwrap() = Some(std::time::Instant::now());
                        let _ = stream.write_all(b"data: {\"event\":\"daemon_stopping\"}\n\n");
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
                        config: vm_4_cfg.clone(),
                        runtime: velovm_core::models::VmRuntimeState {
                            status: VmStatus::Off,
                            revision: 1,
                            ..Default::default()
                        },
                    }]
                } else if cur == 6 {
                    vec![VmSummary {
                        config: vm_5_cfg.clone(),
                        runtime: velovm_core::models::VmRuntimeState {
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
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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
    drop(server);
}

#[test]
fn save_host_config_takes_snapshot_seq_after_patch() {
    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let patch_started = Arc::new(AtomicBool::new(false));
    let unblock_patch = Arc::new(AtomicBool::new(false));
    let patch_applied = Arc::new(AtomicBool::new(false));
    let old_bin = dir.path().join("old-openvmm");
    let new_bin = dir.path().join("new-openvmm");

    let server = {
        let token = token.clone();
        let patch_started = Arc::clone(&patch_started);
        let unblock_patch = Arc::clone(&unblock_patch);
        let patch_applied = Arc::clone(&patch_applied);
        let store_dir = dir.path().to_path_buf();
        let old_bin = old_bin.clone();
        let new_bin = new_bin.clone();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let method = req.method.as_str();
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                if write_sse_ok_headers(&mut stream) {
                    stream_sse_keepalives(&mut stream, stop, Duration::from_millis(25));
                }
                return;
            }
            let (status, body) = if method == "PATCH" && target == "/api/v1/host/config" {
                patch_started.store(true, Ordering::Release);
                while !unblock_patch.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(10));
                }
                patch_applied.store(true, Ordering::Release);
                let mut cfg = HostConfig::default();
                cfg.openvmm_bin = Some(new_bin.clone());
                ("200 OK", serde_json::to_string(&cfg).unwrap())
            } else if method == "GET" && target == "/api/v1/host/config" {
                let mut cfg = HostConfig::default();
                cfg.openvmm_bin = Some(if patch_applied.load(Ordering::Acquire) {
                    new_bin.clone()
                } else {
                    old_bin.clone()
                });
                ("200 OK", serde_json::to_string(&cfg).unwrap())
            } else if target == "/api/v1/vms" {
                ("200 OK", "[]".to_string())
            } else {
                mock_common_endpoint(target, &token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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

    backend.send(GuiRequest::SaveHostConfig {
        dialog_id: Uuid::new_v4(),
        patch: HostConfigPatch {
            openvmm_bin: Some(Some(new_bin.clone())),
            ..Default::default()
        },
    });

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
    drop(server);
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
    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let events_times = Arc::new(std::sync::Mutex::new(Vec::<std::time::Instant>::new()));

    let server = {
        let token = token.clone();
        let events_times = Arc::clone(&events_times);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, _stop| {
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                events_times.lock().unwrap().push(std::time::Instant::now());
                let _ = write_sse_ok_headers(&mut stream);
                // Immediately drop the SSE stream right after serving 200 OK.
                return;
            }
            let (status, body) = if target == "/api/v1/vms" {
                ("200 OK", "[]".to_string())
            } else {
                mock_common_endpoint(target, &token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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
    drop(server);
}

#[test]
fn sse_backoff_resets_after_stream_stays_stable() {
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let sse_attempts = Arc::new(AtomicUsize::new(0));
    let stable_closed_at = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
    let reconnected_at = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));

    let server = {
        let token = token.clone();
        let sse_attempts = Arc::clone(&sse_attempts);
        let stable_closed_at = Arc::clone(&stable_closed_at);
        let reconnected_at = Arc::clone(&reconnected_at);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                let idx = sse_attempts.fetch_add(1, Ordering::SeqCst);
                if idx < 5 {
                    write_mock_json(
                        &mut stream,
                        "500 Internal Server Error",
                        r#"{"error":"transient events error"}"#,
                    );
                    return;
                }
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
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
                    *stable_closed_at.lock().unwrap() = Some(std::time::Instant::now());
                    return;
                }
                if idx == 6 {
                    *reconnected_at.lock().unwrap() = Some(std::time::Instant::now());
                }
                stream_sse_keepalives(&mut stream, stop, Duration::from_millis(20));
                return;
            }
            let (status, body) = if target == "/api/v1/vms" {
                ("200 OK", "[]".to_string())
            } else {
                mock_common_endpoint(target, &token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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
    drop(server);
}

#[test]
fn save_host_config_fetches_fresher_cli_change_after_patch() {
    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let gui_bin = dir.path().join("gui-openvmm");
    let cli_fw = dir.path().join("cli-MSVM.fd");
    let patch_returned = Arc::new(AtomicBool::new(false));

    let server = {
        let token = token.clone();
        let gui_bin = gui_bin.clone();
        let cli_fw = cli_fw.clone();
        let patch_returned = Arc::clone(&patch_returned);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let method = req.method.as_str();
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                if write_sse_ok_headers(&mut stream) {
                    stream_sse_keepalives(&mut stream, stop, Duration::from_millis(25));
                }
                return;
            }
            let (status, body) = if method == "PATCH" && target == "/api/v1/host/config" {
                patch_returned.store(true, Ordering::Release);
                let mut cfg = HostConfig::default();
                cfg.openvmm_bin = Some(gui_bin.clone());
                ("200 OK", serde_json::to_string(&cfg).unwrap())
            } else if method == "GET" && target == "/api/v1/host/config" {
                let mut cfg = HostConfig::default();
                if patch_returned.load(Ordering::Acquire) {
                    // Simulate a CLI change committed immediately after PATCH.
                    cfg.openvmm_bin = Some(gui_bin.clone());
                    cfg.uefi_firmware_path = Some(cli_fw.clone());
                }
                ("200 OK", serde_json::to_string(&cfg).unwrap())
            } else if target == "/api/v1/vms" {
                ("200 OK", "[]".to_string())
            } else {
                mock_common_endpoint(target, &token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
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

    backend.send(GuiRequest::SaveHostConfig {
        dialog_id: Uuid::new_v4(),
        patch: HostConfigPatch {
            openvmm_bin: Some(Some(gui_bin.clone())),
            ..Default::default()
        },
    });

    let updated_cfg = match recv_until(&backend, Duration::from_secs(5), |u| {
        matches!(u, GuiUpdate::HostConfigUpdated { .. })
    }) {
        GuiUpdate::HostConfigUpdated { host_config, .. } => host_config,
        _ => unreachable!(),
    };
    assert_eq!(updated_cfg.openvmm_bin, Some(gui_bin));
    assert_eq!(updated_cfg.uefi_firmware_path, Some(cli_fw));

    drop(backend);
    drop(server);
}

#[test]
fn wake_sse_interrupts_long_backoff_immediately() {
    use velovm_core::models::{VmConfig, VmPreset, VmRuntimeState};
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let fail_events = Arc::new(AtomicBool::new(true));
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
        let store_dir = dir.path().to_path_buf();
        let vm_summary = vm_summary.clone();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                if fail_events.load(Ordering::Acquire) {
                    write_mock_json(
                        &mut stream,
                        "500 Internal Server Error",
                        r#"{"error":"events unavailable"}"#,
                    );
                    return;
                }
                let evt = DaemonEvent::VmUpdated {
                    vm: vm_summary.clone(),
                };
                let frame = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\ndata: {}\n\ndata: {}\n\n",
                    serde_json::to_string(&DaemonEvent::Resync).unwrap(),
                    serde_json::to_string(&evt).unwrap(),
                );
                let _ = stream.write_all(frame.as_bytes());
                let _ = stream.flush();
                stream_sse_keepalives(&mut stream, stop, Duration::from_millis(25));
                return;
            }
            let (status, body) = if target == "/api/v1/vms" {
                ("200 OK", "[]".to_string())
            } else {
                mock_common_endpoint(target, &token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

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
    drop(server);
}

#[cfg(unix)]
#[test]
fn hard_linked_token_surfaces_reason_in_daemon_disconnected() {
    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
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

#[test]
fn healthy_stream_wake_does_not_skip_next_reconnect_backoff() {
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let sse_attempts = Arc::new(AtomicUsize::new(0));
    let drop_first_sse = Arc::new(AtomicBool::new(false));
    let first_closed_at = Arc::new(Mutex::new(None::<std::time::Instant>));
    let second_connected_at = Arc::new(Mutex::new(None::<std::time::Instant>));

    let server = {
        let token = token.clone();
        let sse_attempts = Arc::clone(&sse_attempts);
        let drop_first_sse = Arc::clone(&drop_first_sse);
        let first_closed_at = Arc::clone(&first_closed_at);
        let second_connected_at = Arc::clone(&second_connected_at);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            if req.target == "/api/v1/events" {
                let idx = sse_attempts.fetch_add(1, Ordering::SeqCst);
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
                let _ = stream.write_all(b": keepalive\n\n");
                let _ = stream.flush();
                if idx == 0 {
                    while !stop.load(Ordering::Acquire) && !drop_first_sse.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(10));
                    }
                    *first_closed_at.lock().unwrap() = Some(std::time::Instant::now());
                    return;
                }
                if idx == 1 {
                    *second_connected_at.lock().unwrap() = Some(std::time::Instant::now());
                }
                stream_sse_keepalives(&mut stream, stop, Duration::from_millis(20));
                return;
            }
            let (status, body) = if req.target == "/api/v1/vms" {
                ("200 OK", "[]".to_string())
            } else {
                mock_common_endpoint(&req.target, &token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

    let backend = BackendHandle::spawn_with_sse_timing(
        Some(dir.path().to_path_buf()),
        None,
        egui::Context::default(),
        Duration::from_secs(2),
        Duration::from_millis(250),
        Duration::from_millis(500),
    );
    backend.send(GuiRequest::ConnectOrStartDaemon);
    let _ = recv_until(&backend, Duration::from_secs(3), |u| {
        matches!(u, GuiUpdate::DaemonConnected { .. })
    });
    // Trigger wake_sse while the SSE stream is actively healthy.
    backend.send(GuiRequest::ConnectOrStartDaemon);
    thread::sleep(Duration::from_millis(50));

    // Now drop the healthy stream and verify reconnect still observes >=200ms backoff.
    drop_first_sse.store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while second_connected_at.lock().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for SSE #1"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let gap = second_connected_at
        .lock()
        .unwrap()
        .unwrap()
        .duration_since(first_closed_at.lock().unwrap().unwrap());
    assert!(
        gap >= Duration::from_millis(200),
        "healthy-stream wake must not skip next reconnect backoff (250ms), got {gap:?}"
    );

    drop(backend);
    drop(server);
}

#[test]
fn reconnect_fetches_snapshot_once_without_double_fetch() {
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let fail_events = Arc::new(AtomicBool::new(true));
    let vms_fetch_count = Arc::new(AtomicUsize::new(0));
    let sse_connected = Arc::new(AtomicBool::new(false));

    let server = {
        let token = token.clone();
        let fail_events = Arc::clone(&fail_events);
        let vms_fetch_count = Arc::clone(&vms_fetch_count);
        let sse_connected = Arc::clone(&sse_connected);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            if req.target == "/api/v1/events" {
                if fail_events.load(Ordering::Acquire) {
                    write_mock_json(
                        &mut stream,
                        "500 Internal Server Error",
                        r#"{"error":"not ready"}"#,
                    );
                    return;
                }
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
                let _ = stream.write_all(b": keepalive\n\n");
                let _ = stream.flush();
                sse_connected.store(true, Ordering::Release);
                stream_sse_keepalives(&mut stream, stop, Duration::from_millis(20));
                return;
            }
            let (status, body) = if req.target == "/api/v1/vms" {
                vms_fetch_count.fetch_add(1, Ordering::SeqCst);
                ("200 OK", "[]".to_string())
            } else {
                mock_common_endpoint(&req.target, &token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

    let backend = BackendHandle::spawn_with_sse_timing(
        Some(dir.path().to_path_buf()),
        None,
        egui::Context::default(),
        Duration::from_secs(2),
        Duration::from_secs(10),
        Duration::from_secs(10),
    );
    backend.send(GuiRequest::ConnectOrStartDaemon);
    let _ = recv_until(&backend, Duration::from_secs(3), |u| {
        matches!(u, GuiUpdate::DaemonDisconnected(_))
    });

    // Allow events to succeed, reset counter, and reconnect via ConnectOrStartDaemon.
    vms_fetch_count.store(0, Ordering::SeqCst);
    fail_events.store(false, Ordering::Release);
    backend.send(GuiRequest::ConnectOrStartDaemon);
    let _ = recv_until(&backend, Duration::from_secs(3), |u| {
        matches!(u, GuiUpdate::DaemonConnected { .. })
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !sse_connected.load(Ordering::Acquire) {
        assert!(std::time::Instant::now() < deadline, "SSE did not connect");
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(80));
    assert_eq!(
        vms_fetch_count.load(Ordering::SeqCst),
        1,
        "reconnect should fetch /api/v1/vms once, not twice"
    );

    drop(backend);
    drop(server);
}

#[test]
fn retried_request_on_daemon_switch_updates_instance_id_and_applies_update() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let daemon_a_id = Uuid::new_v4();
    let daemon_b_id = Uuid::new_v4();
    let vm_b_id = Uuid::new_v4();
    let vm_b_summary = VmSummary {
        config: velovm_core::models::VmConfig::from_preset(
            "SwitchedDaemon-VM",
            VmPreset::Custom,
            &dir.path().join("vms").join(vm_b_id.to_string()),
        ),
        runtime: Default::default(),
    };

    let spawn_daemon = |inst_id: Uuid, vm_summary: VmSummary| {
        let token = token.clone();
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            if req.target == "/api/v1/events" {
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
                let _ = stream.write_all(b": keepalive\n\n");
                let _ = stream.flush();
                stream_sse_keepalives(&mut stream, stop, Duration::from_millis(20));
                return;
            }
            let (status, body) = if req.target == "/api/v1/host/info" {
                let info = HostInfo {
                    hostname: "test".into(),
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    cpu_count: 4,
                    daemon_version: "0.1.0".into(),
                    data_dir: store_dir.clone(),
                    runtime_dir: store_dir.clone(),
                    listen_addr: addr.to_string(),
                    daemon_instance_id: Some(inst_id),
                    event_revision: 1,
                    resolved_paths: Default::default(),
                };
                ("200 OK", serde_json::to_string(&info).unwrap())
            } else if req.method == "POST" && req.target == "/api/v1/vms" {
                ("200 OK", serde_json::to_string(&vm_summary).unwrap())
            } else if req.target == "/api/v1/vms" {
                ("200 OK", "[]".to_string())
            } else {
                mock_common_endpoint(&req.target, &token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };

    let srv_a = spawn_daemon(daemon_a_id, vm_b_summary.clone());
    store
        .write_runtime_listen_addr(&srv_a.addr.to_string())
        .unwrap();

    let backend = BackendHandle::spawn(
        Some(dir.path().to_path_buf()),
        None,
        egui::Context::default(),
    );
    let mut app =
        crate::VeloVmApp::new_for_test(&egui::Context::default(), dir.path().to_path_buf());

    backend.send(GuiRequest::ConnectOrStartDaemon);
    let connected_a = recv_until(&backend, Duration::from_secs(3), |u| {
        matches!(u, GuiUpdate::DaemonConnected { .. })
    });
    app.apply_update(connected_a);

    // Shut down daemon A and spin up daemon B at a new address in daemon.addr.
    drop(srv_a);

    let srv_b = spawn_daemon(daemon_b_id, vm_b_summary.clone());
    store
        .write_runtime_listen_addr(&srv_b.addr.to_string())
        .unwrap();

    backend.send(GuiRequest::CreateVm {
        wizard_id: Uuid::new_v4(),
        payload: velovm_core::models::CreateVmPayload {
            name: "SwitchedDaemon-VM".into(),
            preset: VmPreset::Custom,
            ..Default::default()
        },
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_b_vm_updated = false;
    while std::time::Instant::now() < deadline {
        if let Some(upd) = backend.try_recv() {
            if let GuiUpdate::VmUpdated {
                daemon_instance_id, ..
            } = &upd
            {
                assert_eq!(*daemon_instance_id, Some(daemon_b_id));
                saw_b_vm_updated = true;
            }
            app.apply_update(upd);
            if saw_b_vm_updated && app.vms.iter().any(|v| v.config.name == "SwitchedDaemon-VM") {
                break;
            }
        } else {
            thread::sleep(Duration::from_millis(10));
        }
    }
    assert!(
        saw_b_vm_updated,
        "expected retried CreateVm to emit VmUpdated tagged with daemon B's instance ID"
    );
    assert!(
        app.vms.iter().any(|v| v.config.name == "SwitchedDaemon-VM"),
        "VeloVmApp should apply the retried VM update from daemon B"
    );

    drop(backend);
    drop(srv_b);
}

#[test]
fn forced_refresh_does_not_stamp_newer_epoch_and_token_change_resyncs() {
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token1 = "token-one".to_string();
    let token2 = "token-two".to_string();
    let active_token = Arc::new(Mutex::new(token1.clone()));

    let (vms_entered_tx, vms_entered_rx) = std::sync::mpsc::channel::<()>();
    let (vms_release_tx, vms_release_rx) = std::sync::mpsc::channel::<()>();
    let pause_first_vms = Arc::new(AtomicBool::new(true));
    let vms_count = Arc::new(AtomicUsize::new(0));

    let server = {
        let active_token = Arc::clone(&active_token);
        let pause_first_vms = Arc::clone(&pause_first_vms);
        let vms_count = Arc::clone(&vms_count);
        let store_dir = dir.path().to_path_buf();
        let vms_entered_tx = Arc::new(Mutex::new(Some(vms_entered_tx)));
        let vms_release_rx = Arc::new(Mutex::new(vms_release_rx));
        TestMockServer::spawn(move |mut stream, req, addr, _| {
            if req.target == "/api/v1/vms" {
                vms_count.fetch_add(1, Ordering::SeqCst);
                if pause_first_vms.swap(false, Ordering::AcqRel) {
                    if let Some(tx) = vms_entered_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    let _ = vms_release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5));
                }
                write_mock_json(&mut stream, "200 OK", "[]");
                return;
            }
            let tok = active_token.lock().unwrap().clone();
            let (status, body) = mock_common_endpoint(&req.target, &tok, addr, &store_dir)
                .unwrap_or(("404 Not Found", "{}".into()));
            write_mock_json(&mut stream, status, &body);
        })
    };

    let (upd_tx, _upd_rx) = mpsc::channel();
    let state = Arc::new(BackendState {
        data_dir: Some(dir.path().to_path_buf()),
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
        sse_read_timeout: Duration::from_secs(2),
        sse_backoff_initial: Duration::from_millis(50),
        sse_backoff_max: Duration::from_secs(1),
        sse_backoff_stable_after: Duration::from_secs(5),
        upd_tx,
        repaint_ctx: egui::Context::default(),
    });

    let c1 = ApiClient::new(format!("http://{}", server.addr), token1).unwrap();
    let state_bg = Arc::clone(&state);
    let c1_bg = c1.clone();
    let h = thread::spawn(move || state_bg.sync_snapshot_once(&c1_bg, true));

    vms_entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    state.invalidate_snapshot_sync();
    let _ = vms_release_tx.send(());

    let outcome1 = h.join().unwrap().unwrap();
    assert!(outcome1.fetched);
    assert!(
        state.snapshot_sync.lock().unwrap().synced.is_none(),
        "forced refresh interrupted by invalidate_snapshot_sync must not stamp the newer epoch"
    );

    // Next non-forced sync on same URL + token1 must fetch because epoch was not stamped.
    let outcome2 = state.sync_snapshot_once(&c1, false).unwrap();
    assert!(outcome2.fetched);
    // Same URL + token1 while still connected returns cached without fetching.
    let outcome3 = state.sync_snapshot_once(&c1, false).unwrap();
    assert!(!outcome3.fetched);
    // Token rotation on the same base_url must re-fetch.
    *active_token.lock().unwrap() = token2.clone();
    let c2 = ApiClient::new(format!("http://{}", server.addr), token2).unwrap();
    let outcome4 = state.sync_snapshot_once(&c2, false).unwrap();
    assert!(outcome4.fetched);
    assert_eq!(vms_count.load(Ordering::SeqCst), 3);

    drop(server);
}

#[test]
fn pre_sse_refresh_does_not_suppress_leading_resync_and_retry_refresh_fetches_once() {
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let daemon_a_id = Uuid::new_v4();
    let daemon_b_id = Uuid::new_v4();
    let sse_ready = Arc::new(AtomicBool::new(false));
    let vms_count_a = Arc::new(AtomicUsize::new(0));
    let vms_count_b = Arc::new(AtomicUsize::new(0));

    let srv_a = {
        let token = token.clone();
        let sse_ready = Arc::clone(&sse_ready);
        let vms_count_a = Arc::clone(&vms_count_a);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr_a, stop_a| {
            if req.target == "/api/v1/events" {
                if !sse_ready.load(Ordering::Acquire) {
                    write_mock_json(&mut stream, "503 Service Unavailable", "{}");
                    return;
                }
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
                let frame = format!(
                    "data: {}\n\n",
                    serde_json::to_string(&DaemonEvent::Resync).unwrap(),
                );
                if stream.write_all(frame.as_bytes()).is_err() {
                    return;
                }
                let _ = stream.flush();
                stream_sse_keepalives(&mut stream, stop_a, Duration::from_millis(20));
                return;
            }
            let (status, body) = if req.target == "/api/v1/host/info" {
                let info = HostInfo {
                    hostname: "test".into(),
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    cpu_count: 4,
                    daemon_version: "0.1.0".into(),
                    data_dir: store_dir.clone(),
                    runtime_dir: store_dir.clone(),
                    listen_addr: addr_a.to_string(),
                    daemon_instance_id: Some(daemon_a_id),
                    event_revision: 1,
                    resolved_paths: Default::default(),
                };
                ("200 OK", serde_json::to_string(&info).unwrap())
            } else if req.target == "/api/v1/vms" {
                vms_count_a.fetch_add(1, Ordering::SeqCst);
                ("200 OK", "[]".to_string())
            } else {
                mock_common_endpoint(&req.target, &token, addr_a, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };
    store
        .write_runtime_listen_addr(&srv_a.addr.to_string())
        .unwrap();

    let backend = BackendHandle::spawn_with_sse_timing(
        Some(dir.path().to_path_buf()),
        None,
        egui::Context::default(),
        Duration::from_secs(2),
        Duration::from_secs(10),
        Duration::from_secs(10),
    );
    backend.send(GuiRequest::ConnectOrStartDaemon);
    let _ = recv_until(&backend, Duration::from_secs(3), |u| {
        matches!(u, GuiUpdate::DaemonDisconnected(_))
    });

    // Pre-SSE RefreshAll while disconnected fetches once (1), then wakes SSE;
    // once SSE connects and sends its initial Resync, SSE must NOT skip it (count reaches 2).
    sse_ready.store(true, Ordering::Release);
    backend.send(GuiRequest::RefreshAll);
    let _ = recv_until(&backend, Duration::from_secs(3), |u| {
        matches!(
            u,
            GuiUpdate::DaemonConnected {
                snapshot_seq: 1,
                ..
            }
        )
    });
    let _ = recv_until(&backend, Duration::from_secs(3), |u| {
        matches!(
            u,
            GuiUpdate::DaemonConnected {
                snapshot_seq: 2,
                ..
            }
        )
    });
    assert_eq!(vms_count_a.load(Ordering::SeqCst), 2);

    // Now test post-error retry on mid-flight daemon switch using FetchVmDetails + RefreshAll:
    // Start daemon B, keep a daemon A handler open to fail mid-flight after daemon.addr flips to B.
    drop(srv_a);

    let vm_b_id = Uuid::new_v4();
    let mut vm_b_cfg = velovm_core::models::VmConfig::from_preset(
        "MidFlightSwitched-VM",
        VmPreset::Custom,
        &dir.path().join("vms").join(vm_b_id.to_string()),
    );
    vm_b_cfg.id = vm_b_id;
    let vm_b_summary = VmSummary {
        config: vm_b_cfg,
        runtime: Default::default(),
    };

    let (mid_entered_tx, mid_entered_rx) = std::sync::mpsc::channel::<()>();
    let (mid_release_tx, mid_release_rx) = std::sync::mpsc::channel::<()>();
    let srv_mid = {
        let token = token.clone();
        let store_dir = dir.path().to_path_buf();
        let mid_entered_tx = Arc::new(Mutex::new(Some(mid_entered_tx)));
        let mid_release_rx = Arc::new(Mutex::new(mid_release_rx));
        TestMockServer::spawn(move |mut stream, req, addr_mid, stop_mid| {
            if req.target == "/api/v1/events" {
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
                let _ = stream.write_all(b": keepalive\n\n");
                let _ = stream.flush();
                stream_sse_keepalives(&mut stream, stop_mid, Duration::from_millis(20));
                return;
            }
            if req.target.starts_with(&format!("/api/v1/vms/{vm_b_id}")) {
                if let Some(tx) = mid_entered_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let _ = mid_release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5));
                // Drop connection without HTTP response to force mid-flight error + post-error retry.
                return;
            }
            let (status, body) = if req.target == "/api/v1/vms" {
                ("200 OK", "[]".to_string())
            } else {
                mock_common_endpoint(&req.target, &token, addr_mid, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };

    let srv_b = {
        let token = token.clone();
        let vms_count_b = Arc::clone(&vms_count_b);
        let store_dir = dir.path().to_path_buf();
        let vm_b_summary = vm_b_summary.clone();
        TestMockServer::spawn(move |mut stream, req, addr_b, stop_b| {
            if req.target == "/api/v1/events" {
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
                let _ = stream.write_all(b": keepalive\n\n");
                let _ = stream.flush();
                stream_sse_keepalives(&mut stream, stop_b, Duration::from_millis(20));
                return;
            }
            let (status, body) = if req.target == "/api/v1/host/info" {
                let info = HostInfo {
                    hostname: "test".into(),
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    cpu_count: 4,
                    daemon_version: "0.1.0".into(),
                    data_dir: store_dir.clone(),
                    runtime_dir: store_dir.clone(),
                    listen_addr: addr_b.to_string(),
                    daemon_instance_id: Some(daemon_b_id),
                    event_revision: 1,
                    resolved_paths: Default::default(),
                };
                ("200 OK", serde_json::to_string(&info).unwrap())
            } else if req.target == "/api/v1/vms" {
                vms_count_b.fetch_add(1, Ordering::SeqCst);
                (
                    "200 OK",
                    serde_json::to_string(&vec![vm_b_summary.clone()]).unwrap(),
                )
            } else if req.target == format!("/api/v1/vms/{vm_b_id}") {
                ("200 OK", serde_json::to_string(&vm_b_summary).unwrap())
            } else {
                mock_common_endpoint(&req.target, &token, addr_b, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };

    store
        .write_runtime_listen_addr(&srv_mid.addr.to_string())
        .unwrap();
    let backend2 = BackendHandle::spawn(
        Some(dir.path().to_path_buf()),
        None,
        egui::Context::default(),
    );
    backend2.send(GuiRequest::ConnectOrStartDaemon);
    let _ = recv_until(&backend2, Duration::from_secs(3), |u| {
        matches!(u, GuiUpdate::DaemonConnected { .. })
    });

    // Send FetchVmDetails while daemon.addr points to mid; flip daemon.addr to B mid-flight before failing mid.
    backend2.send(GuiRequest::FetchVmDetails { id: vm_b_id });
    mid_entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    store
        .write_runtime_listen_addr(&srv_b.addr.to_string())
        .unwrap();
    let _ = mid_release_tx.send(());

    let updated = recv_until(&backend2, Duration::from_secs(3), |u| {
        matches!(
            u,
            GuiUpdate::VmUpdated {
                is_details_fetch: true,
                daemon_instance_id: Some(inst),
                ..
            } if *inst == daemon_b_id
        )
    });
    assert!(matches!(
        updated,
        GuiUpdate::VmUpdated {
            daemon_instance_id: Some(inst),
            ..
        } if inst == daemon_b_id
    ));

    // Also verify RefreshAll post-error switched-endpoint retry fetches /api/v1/vms only once.
    let (mid2_entered_tx, mid2_entered_rx) = std::sync::mpsc::channel::<()>();
    let (mid2_release_tx, mid2_release_rx) = std::sync::mpsc::channel::<()>();
    let fail_mid2_vms = Arc::new(AtomicBool::new(false));
    let srv_mid2 = {
        let token = token.clone();
        let fail_mid2_vms = Arc::clone(&fail_mid2_vms);
        let store_dir = dir.path().to_path_buf();
        let mid2_entered_tx = Arc::new(Mutex::new(Some(mid2_entered_tx)));
        let mid2_release_rx = Arc::new(Mutex::new(mid2_release_rx));
        TestMockServer::spawn(move |mut stream, req, addr_mid2, stop_mid2| {
            if req.target == "/api/v1/events" {
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
                let _ = stream.write_all(b": keepalive\n\n");
                let _ = stream.flush();
                stream_sse_keepalives(&mut stream, stop_mid2, Duration::from_millis(20));
                return;
            }
            if req.target == "/api/v1/vms" {
                if fail_mid2_vms.load(Ordering::Acquire) {
                    if let Some(tx) = mid2_entered_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    let _ = mid2_release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5));
                    return;
                }
                write_mock_json(&mut stream, "200 OK", "[]");
                return;
            }
            let (status, body) = mock_common_endpoint(&req.target, &token, addr_mid2, &store_dir)
                .unwrap_or(("404 Not Found", "{}".into()));
            write_mock_json(&mut stream, status, &body);
        })
    };

    store
        .write_runtime_listen_addr(&srv_mid2.addr.to_string())
        .unwrap();
    let backend3 = BackendHandle::spawn(
        Some(dir.path().to_path_buf()),
        None,
        egui::Context::default(),
    );
    backend3.send(GuiRequest::ConnectOrStartDaemon);
    let _ = recv_until(&backend3, Duration::from_secs(3), |u| {
        matches!(u, GuiUpdate::DaemonConnected { .. })
    });
    vms_count_b.store(0, Ordering::SeqCst);
    fail_mid2_vms.store(true, Ordering::Release);
    backend3.send(GuiRequest::RefreshAll);
    mid2_entered_rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    store
        .write_runtime_listen_addr(&srv_b.addr.to_string())
        .unwrap();
    let _ = mid2_release_tx.send(());
    let _ = recv_until(&backend3, Duration::from_secs(3), |u| {
        matches!(
            u,
            GuiUpdate::DaemonConnected {
                host_info,
                ..
            } if host_info.daemon_instance_id == Some(daemon_b_id)
        )
    });
    thread::sleep(Duration::from_millis(80));
    assert_eq!(
        vms_count_b.load(Ordering::SeqCst),
        1,
        "switched-endpoint RefreshAll retry must fetch snapshot once, not twice"
    );

    drop(backend);
    drop(backend2);
    drop(backend3);
    drop(srv_mid);
    drop(srv_mid2);
    drop(srv_b);
}

#[test]
fn sse_keepalives_prevent_total_request_timeout_while_silence_still_triggers_idle_timeout() {
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();
    let token = store.load_or_create_token().unwrap();

    let sse_count = Arc::new(AtomicUsize::new(0));
    let vms_fetches = Arc::new(AtomicUsize::new(0));
    let vms_stage = Arc::new(AtomicUsize::new(0));

    let vm_id = Uuid::new_v4();
    let mut base_cfg = velovm_core::models::VmConfig::from_preset(
        "KeepAlive-VM",
        VmPreset::Custom,
        Path::new("/tmp/vm"),
    );
    base_cfg.id = vm_id;

    let server = {
        let token = token.clone();
        let sse_count = Arc::clone(&sse_count);
        let vms_fetches = Arc::clone(&vms_fetches);
        let vms_stage = Arc::clone(&vms_stage);
        let store_dir = dir.path().to_path_buf();
        let cfg_for_srv = base_cfg.clone();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let target = req.target.as_str();
            if target == "/api/v1/events" {
                let idx = sse_count.fetch_add(1, Ordering::SeqCst) + 1;
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
                let leading_json = serde_json::to_string(&DaemonEvent::Resync).unwrap();
                let _ = stream.write_all(format!("data: {leading_json}\n\n").as_bytes());
                let _ = stream.flush();
                if idx == 1 {
                    // Send keep-alives every 100ms for 700ms (~3x the 250ms read timeout).
                    for _ in 0..7 {
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(100));
                        if stream.write_all(b": keep-alive\n\n").is_err() {
                            return;
                        }
                        let _ = stream.flush();
                    }
                    let updated_vm = VmSummary {
                        config: cfg_for_srv.clone(),
                        runtime: velovm_core::models::VmRuntimeState {
                            status: VmStatus::Running,
                            revision: 2,
                            ..Default::default()
                        },
                    };
                    let ev_json =
                        serde_json::to_string(&DaemonEvent::VmUpdated { vm: updated_vm }).unwrap();
                    let _ = stream.write_all(format!("data: {ev_json}\n\n").as_bytes());
                    let _ = stream.flush();
                    // Now go completely silent so idle read_timeout fires and triggers reconnect/reconcile.
                    vms_stage.store(1, Ordering::Release);
                }
                while !stop.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(25));
                }
                return;
            }

            let (status, body) = if target == "/api/v1/vms" {
                vms_fetches.fetch_add(1, Ordering::SeqCst);
                let stage = vms_stage.load(Ordering::Acquire);
                let summary = VmSummary {
                    config: cfg_for_srv.clone(),
                    runtime: velovm_core::models::VmRuntimeState {
                        status: if stage > 0 {
                            VmStatus::Paused
                        } else {
                            VmStatus::Off
                        },
                        revision: (stage as u64) + 1,
                        ..Default::default()
                    },
                };
                ("200 OK", serde_json::to_string(&vec![summary]).unwrap())
            } else {
                mock_common_endpoint(target, &token, addr, &store_dir)
                    .unwrap_or(("404 Not Found", "{}".into()))
            };
            write_mock_json(&mut stream, status, &body);
        })
    };

    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

    let backend = BackendHandle::spawn_with_sse_timeout(
        Some(dir.path().to_path_buf()),
        None,
        egui::Context::default(),
        Duration::from_millis(250),
    );
    backend.send(GuiRequest::ConnectOrStartDaemon);

    let _ = recv_until(&backend, Duration::from_secs(5), |u| {
        matches!(u, GuiUpdate::DaemonConnected { .. })
    });

    // VmUpdated arrives after ~700ms of keep-alives without any reconnect or second snapshot fetch.
    let updated = recv_until(&backend, Duration::from_secs(5), |u| {
        matches!(
            u,
            GuiUpdate::VmUpdated { vm: v, .. } if v.runtime.status == VmStatus::Running
        )
    });
    assert!(matches!(
        updated,
        GuiUpdate::VmUpdated { vm: v, .. } if v.runtime.revision == 2
    ));
    assert_eq!(sse_count.load(Ordering::SeqCst), 1);
    assert_eq!(vms_fetches.load(Ordering::SeqCst), 1);

    // Once keep-alives stop, idle read_timeout fires and reconnects + reconciles to stage 1 (Paused).
    let reconciled = recv_until(&backend, Duration::from_secs(5), |u| {
        matches!(u, GuiUpdate::DaemonConnected { vms, .. } if vms.iter().any(|v| v.runtime.status == VmStatus::Paused))
    });
    assert!(matches!(reconciled, GuiUpdate::DaemonConnected { .. }));
    assert!(sse_count.load(Ordering::SeqCst) >= 2);
}

#[test]
fn sse_line_buffer_caps_unbounded_growth_and_strips_eof_cr() {
    let mut lb = SseLineBuffer::new(16);
    assert!(lb.push_chunk(b"data: ok\r\n"));
    assert_eq!(lb.take_line().as_deref(), Some("data: ok"));
    assert_eq!(lb.take_line(), None);

    // Trailing CR at EOF without LF must be stripped consistently with take_line().
    assert!(lb.push_chunk(b"data: tail\r"));
    assert_eq!(lb.take_line(), None);
    assert_eq!(lb.finish().as_deref(), Some("data: tail"));
    assert_eq!(lb.finish(), None);

    // Overflowing max_bytes without newline rejects and clears buffer so listener reconnects.
    assert!(lb.push_chunk(b"0123456789"));
    assert!(!lb.push_chunk(b"abcdefg"));
    assert_eq!(lb.finish(), None);
}

#[test]
fn token_rotation_on_same_url_switches_sse_without_backoff_and_does_not_clobber_client() {
    let dir = tempfile::tempdir().unwrap();
    let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
    let _lock = store.try_acquire_daemon_lock().unwrap();

    let token1 = store.load_or_create_token().unwrap();
    let daemon1_id = Uuid::new_v4();
    let daemon2_id = Uuid::new_v4();
    let active_token = Arc::new(Mutex::new(token1.clone()));
    let active_instance = Arc::new(Mutex::new(daemon1_id));
    let drop_sse1 = Arc::new(AtomicBool::new(false));
    let sse_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (sse1_connected_tx, sse1_connected_rx) = mpsc::channel::<()>();
    let sse1_connected_tx = Arc::new(Mutex::new(Some(sse1_connected_tx)));
    let (pause_entered_tx, pause_entered_rx) = mpsc::channel::<()>();
    let pause_entered_tx = Arc::new(Mutex::new(Some(pause_entered_tx)));
    let (pause_release_tx, pause_release_rx) = mpsc::channel::<()>();
    let pause_release_rx = Arc::new(Mutex::new(pause_release_rx));

    let server = {
        let active_token = Arc::clone(&active_token);
        let active_instance = Arc::clone(&active_instance);
        let drop_sse1 = Arc::clone(&drop_sse1);
        let sse_count = Arc::clone(&sse_count);
        let sse1_connected_tx = Arc::clone(&sse1_connected_tx);
        let pause_entered_tx = Arc::clone(&pause_entered_tx);
        let pause_release_rx = Arc::clone(&pause_release_rx);
        let store_dir = dir.path().to_path_buf();
        TestMockServer::spawn(move |mut stream, req, addr, stop| {
            let tok = active_token.lock().unwrap().clone();
            let inst = *active_instance.lock().unwrap();
            if req.target == "/api/v1/events" {
                let conn_num = sse_count.fetch_add(1, Ordering::SeqCst);
                if !write_sse_ok_headers(&mut stream) {
                    return;
                }
                if conn_num == 0 {
                    if let Some(tx) = sse1_connected_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    use std::io::Write;
                    while !stop.load(Ordering::Acquire) && !drop_sse1.load(Ordering::Acquire) {
                        if stream.write_all(b": keepalive\n\n").is_err() {
                            break;
                        }
                        let _ = stream.flush();
                        thread::sleep(Duration::from_millis(15));
                    }
                } else {
                    stream_sse_keepalives(&mut stream, stop, Duration::from_millis(15));
                }
                return;
            }
            if req.target == "/api/v1/host/info" {
                let info = HostInfo {
                    hostname: "test".into(),
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    cpu_count: 4,
                    daemon_version: "0.1.0".into(),
                    data_dir: store_dir.clone(),
                    runtime_dir: store_dir.clone(),
                    listen_addr: addr.to_string(),
                    daemon_instance_id: Some(inst),
                    event_revision: 1,
                    resolved_paths: Default::default(),
                };
                write_mock_json(&mut stream, "200 OK", &serde_json::to_string(&info).unwrap());
                return;
            }
            if req.target == "/api/v1/vms" {
                write_mock_json(&mut stream, "200 OK", "[]");
                return;
            }
            if req.target.ends_with("/pause") {
                if let Some(tx) = pause_entered_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let _ = pause_release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5));
                return;
            }
            let (status, body) = mock_common_endpoint(&req.target, &tok, addr, &store_dir)
                .unwrap_or(("404 Not Found", "{}".into()));
            write_mock_json(&mut stream, status, &body);
        })
    };

    let url = format!("http://{}", server.addr);
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();

    // Use a huge SSE initial backoff (10s): detecting same-URL token rotation must bypass backoff.
    let backend = BackendHandle::spawn_with_sse_timing(
        Some(dir.path().to_path_buf()),
        None,
        egui::Context::default(),
        Duration::from_secs(5),
        Duration::from_secs(10),
        Duration::from_secs(10),
    );
    backend.send(GuiRequest::ConnectOrStartDaemon);
    match recv_until(&backend, Duration::from_secs(3), |u| {
        matches!(u, GuiUpdate::DaemonConnected { .. })
    }) {
        GuiUpdate::DaemonConnected { host_info, .. } => {
            assert_eq!(host_info.daemon_instance_id, Some(daemon1_id));
        }
        _ => unreachable!(),
    }
    sse1_connected_rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap();

    // Rotate daemon.token on the exact same listen_addr and drop the old SSE stream.
    std::fs::remove_file(store.token_path()).unwrap();
    let token2 = store.load_or_create_token().unwrap();
    assert_ne!(token1, token2);
    *active_token.lock().unwrap() = token2.clone();
    *active_instance.lock().unwrap() = daemon2_id;
    store
        .write_runtime_listen_addr(&server.addr.to_string())
        .unwrap();
    drop_sse1.store(true, Ordering::Release);

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut switched = false;
    while std::time::Instant::now() < deadline && !switched {
        while let Some(upd) = backend.try_recv() {
            match upd {
                GuiUpdate::DaemonDisconnected(reason) => {
                    panic!("same-URL token rotation triggered unexpected DaemonDisconnected: {reason}");
                }
                GuiUpdate::DaemonConnected { host_info, .. }
                    if host_info.daemon_instance_id == Some(daemon2_id) =>
                {
                    switched = true;
                    break;
                }
                _ => {}
            }
        }
        if !switched {
            thread::sleep(Duration::from_millis(15));
        }
    }
    assert!(
        switched,
        "SSE listener did not reconnect promptly with rotated token on same URL"
    );

    // Also verify a failing stale-token request cannot clear state.client once rotated to token2.
    let (upd_tx, _upd_rx) = mpsc::channel();
    let state = Arc::new(BackendState {
        data_dir: Some(dir.path().to_path_buf()),
        daemon_url: None,
        client: RwLock::new(Some(ApiClient::new(&url, &token1).unwrap())),
        shared_endpoint: Arc::new(RwLock::new(Some((url.clone(), token1.clone())))),
        sse_active: AtomicBool::new(false),
        notified_connected: AtomicBool::new(true),
        shutdown: AtomicBool::new(false),
        sse_wake: (Mutex::new(0), Condvar::new()),
        connect_in_flight: AtomicBool::new(false),
        snapshot_seq: std::sync::atomic::AtomicU64::new(0),
        snapshot_sync_lock: Mutex::new(()),
        snapshot_sync: Mutex::new(SnapshotSyncState::default()),
        daemon_instance_id: Mutex::new((0, Some(daemon1_id))),
        host_cfg_lock: Mutex::new(()),
        logs_in_flight: Mutex::new(HashSet::new()),
        details_in_flight: Mutex::new(HashSet::new()),
        attach_disk_in_flight: Mutex::new(HashSet::new()),
        mount_iso_in_flight: Mutex::new(HashSet::new()),
        sse_read_timeout: Duration::from_secs(2),
        sse_backoff_initial: Duration::from_millis(50),
        sse_backoff_max: Duration::from_secs(1),
        sse_backoff_stable_after: Duration::from_secs(5),
        upd_tx,
        repaint_ctx: egui::Context::default(),
    });

    let state_req = Arc::clone(&state);
    let vm_id = Uuid::new_v4();
    let req_thread = thread::spawn(move || {
        state_req.process_request(GuiRequest::PauseVm { id: vm_id });
    });
    pause_entered_rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap();

    // Another thread rotates state.client / shared_endpoint to (same_url, token2) via disconnect helper.
    assert!(maybe_switch_endpoint_on_disconnect(&url, &token1, &state).unwrap());
    assert_eq!(
        state
            .client
            .read()
            .unwrap()
            .as_ref()
            .map(|c| c.token().to_string()),
        Some(token2.clone())
    );

    // Stale token1 request now fails (stream dropped + health fails auth proof against token2).
    let _ = pause_release_tx.send(());
    req_thread.join().unwrap();

    assert_eq!(
        state
            .client
            .read()
            .unwrap()
            .as_ref()
            .map(|c| c.token().to_string()),
        Some(token2),
        "stale failing token1 request must not clear rotated token2 client"
    );

    drop(backend);
    drop(server);
}

#[test]
fn reload_client_from_runtime_addr_respects_daemon_url_when_unconnected() {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let daemon = rt
        .block_on(velovmd::TestDaemonHandle::spawn(dir.path().to_path_buf()))
        .unwrap();
    let store = velovm_core::store::RegistryStore::open(dir.path());
    let token = store.read_token().unwrap();

    // Reserve a different unused loopback port for --daemon-url.
    let unused_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other_addr = unused_listener.local_addr().unwrap();
    let override_url = format!("http://{other_addr}");
    assert_ne!(override_url, daemon.base_url);

    // 1. When shared_endpoint is None and daemon.addr points to a different port,
    //    reload must NOT bypass --daemon-url.
    let shared: SharedEndpoint = Arc::new(RwLock::new(None));
    let res = reload_client_from_runtime_addr_result(
        Some(dir.path()),
        Some(&override_url),
        &shared,
    )
    .unwrap();
    assert!(
        res.is_none(),
        "must not adopt runtime daemon.addr when --daemon-url targets a different endpoint"
    );
    assert!(shared.read().unwrap().is_none());

    // 2. When --daemon-url matches the runtime daemon.addr endpoint (even without scheme),
    //    reload is permitted when unconnected.
    let bare_runtime_addr = daemon.base_url.trim_start_matches("http://");
    let res_same = reload_client_from_runtime_addr_result(
        Some(dir.path()),
        Some(bare_runtime_addr),
        &shared,
    )
    .unwrap();
    let adopted = res_same.expect("matching --daemon-url should be adopted");
    assert_eq!(adopted.base_url(), daemon.base_url);

    // 3. When shared_endpoint already holds the matching token, reload still allows
    //    following a runtime address update under --daemon-url.
    let shared_with_token: SharedEndpoint =
        Arc::new(RwLock::new(Some((override_url.clone(), token))));
    let res_token = reload_client_from_runtime_addr_result(
        Some(dir.path()),
        Some(&override_url),
        &shared_with_token,
    )
    .unwrap();
    assert!(res_token.is_some());
}

#[test]
fn shutdown_vm_request_rejects_acpi_with_force_before_rpc() {
    let (upd_tx, _upd_rx) = mpsc::channel();
    let mut client = ApiClient::new("http://127.0.0.1:1", "tok").unwrap();
    let state = BackendState {
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
        sse_read_timeout: Duration::from_secs(2),
        sse_backoff_initial: Duration::from_millis(50),
        sse_backoff_max: Duration::from_secs(1),
        sse_backoff_stable_after: Duration::from_secs(5),
        upd_tx,
        repaint_ctx: egui::Context::default(),
    };
    let err = state
        .execute_client_request(
            &mut client,
            GuiRequest::ShutdownVm {
                id: Uuid::nil(),
                acpi: true,
                force: true,
            },
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("ACPI shutdown does not support force"),
        "unexpected error: {err:#}"
    );
}
