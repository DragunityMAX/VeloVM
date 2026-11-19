use crate::supervisor::Supervisor;
use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::sse::Event;
use axum::response::sse::KeepAlive;
use axum::response::sse::Sse;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use futures_util::Stream;
use openvmms_core::models::ApiErrorBody;
use openvmms_core::models::AttachDiskPayload;
use openvmms_core::models::CreateDiskPayload;
use openvmms_core::models::CreateVmPayload;
use openvmms_core::models::DaemonEvent;
use openvmms_core::models::DiskInfo;
use openvmms_core::models::HostConfig;
use openvmms_core::models::HostInfo;
use openvmms_core::models::InspectDiskPayload;
use openvmms_core::models::MountIsoPayload;
use openvmms_core::models::PortForwardPayload;
use openvmms_core::models::ShutdownVmPayload;
use openvmms_core::models::UpdateVmPayload;
use openvmms_core::models::VirtualSwitchInfo;
use openvmms_core::models::VmSummary;
use serde::Deserialize;
use serde::Serialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub supervisor: Arc<Supervisor>,
    pub token: String,
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(err: impl ToString) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: err.to_string(),
        }
    }

    fn map_anyhow(err: anyhow::Error) -> Self {
        let msg = format!("{err:#}");
        let status = if msg.contains("not found") {
            StatusCode::NOT_FOUND
        } else if msg.contains("already exists")
            || msg.contains("already Running")
            || msg.contains("is already ")
            || msg.contains("occupied")
            || msg.contains("must be")
            || msg.contains("cannot ")
        {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        Self {
            status,
            message: msg,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

pub fn build_router(supervisor: Arc<Supervisor>, token: String) -> Router {
    let state = AppState { supervisor, token };

    let protected = Router::new()
        .route("/api/v1/events", get(events_handler))
        .route("/api/v1/host/info", get(host_info))
        .route(
            "/api/v1/host/config",
            get(get_host_config).put(put_host_config),
        )
        .route("/api/v1/host/switches", get(list_switches))
        .route("/api/v1/daemon/shutdown", post(daemon_shutdown))
        .route("/api/v1/disks/create", post(create_disk))
        .route("/api/v1/disks/inspect", post(inspect_disk))
        .route("/api/v1/vms", get(list_vms).post(create_vm))
        .route(
            "/api/v1/vms/{id}",
            get(get_vm).put(update_vm).delete(delete_vm),
        )
        .route("/api/v1/vms/{id}/start", post(start_vm))
        .route("/api/v1/vms/{id}/stop", post(stop_vm))
        .route("/api/v1/vms/{id}/shutdown", post(shutdown_vm))
        .route("/api/v1/vms/{id}/pause", post(pause_vm))
        .route("/api/v1/vms/{id}/resume", post(resume_vm))
        .route("/api/v1/vms/{id}/reset", post(reset_vm))
        .route("/api/v1/vms/{id}/disks", post(attach_disk))
        .route("/api/v1/vms/{id}/disks/{lun}", delete(detach_disk))
        .route("/api/v1/vms/{id}/iso", post(mount_iso).delete(eject_iso_default))
        .route("/api/v1/vms/{id}/iso/{lun}", delete(eject_iso_lun))
        .route(
            "/api/v1/vms/{id}/nics/{nic_id}/ports",
            post(add_port_forward).delete(remove_port_forward),
        )
        .route("/api/v1/vms/{id}/logs", get(vm_logs))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .route("/api/v1/health", get(health))
        .merge(protected)
        .with_state(state)
}

async fn require_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let header_token = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));

    let query_token = request.uri().query().and_then(|q| {
        q.split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| *k == "token")
            .map(|(_, v)| v)
    });

    let candidate = header_token.or(query_token);
    if candidate.is_some_and(|t| constant_time_eq(t.as_bytes(), state.token.as_bytes())) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Deserialize)]
struct HealthQuery {
    challenge: Option<String>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_proof: Option<String>,
}

async fn health(
    State(state): State<AppState>,
    Query(query): Query<HealthQuery>,
) -> Json<HealthResponse> {
    let auth_proof = query
        .challenge
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(|c| openvmms_core::models::compute_health_auth_proof(&state.token, c));
    Json(HealthResponse {
        status: "ok",
        service: "openvmmsd",
        auth_proof,
    })
}

async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.supervisor.subscribe();
    let shutdown_rx = state.supervisor.subscribe_shutdown();
    let stream = futures_util::stream::unfold(
        (rx, shutdown_rx, false),
        |(mut rx, mut shutdown_rx, done)| async move {
            if done || *shutdown_rx.borrow() {
                return None;
            }
            loop {
                tokio::select! {
                    item = rx.recv() => {
                        match item {
                            Ok(event) => {
                                let is_stopping = matches!(event, DaemonEvent::DaemonStopping);
                                if let Ok(json) = serde_json::to_string(&event) {
                                    return Some((
                                        Ok::<Event, Infallible>(Event::default().data(json)),
                                        (rx, shutdown_rx, is_stopping),
                                    ));
                                }
                                if is_stopping {
                                    return None;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            while let Ok(event) = rx.try_recv() {
                                if matches!(event, DaemonEvent::DaemonStopping)
                                    && let Ok(json) = serde_json::to_string(&event)
                                {
                                    return Some((
                                        Ok::<Event, Infallible>(Event::default().data(json)),
                                        (rx, shutdown_rx, true),
                                    ));
                                }
                            }
                            return None;
                        }
                    }
                }
            }
        },
    );
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

async fn host_info(State(state): State<AppState>) -> Json<HostInfo> {
    Json(state.supervisor.host_info())
}

async fn get_host_config(State(state): State<AppState>) -> Json<HostConfig> {
    Json(state.supervisor.host_config())
}

async fn put_host_config(
    State(state): State<AppState>,
    Json(cfg): Json<HostConfig>,
) -> Result<impl IntoResponse, ApiError> {
    let updated = state
        .supervisor
        .update_host_config(cfg)
        .await
        .map_err(ApiError::map_anyhow)?;
    Ok(([(axum::http::header::CONNECTION, "close")], Json(updated)))
}

async fn list_switches(State(state): State<AppState>) -> Json<Vec<VirtualSwitchInfo>> {
    Json(state.supervisor.list_switches())
}

async fn daemon_shutdown(State(state): State<AppState>) -> StatusCode {
    state.supervisor.request_shutdown();
    StatusCode::ACCEPTED
}

async fn create_disk(
    State(state): State<AppState>,
    Json(req): Json<CreateDiskPayload>,
) -> Result<Json<DiskInfo>, ApiError> {
    tokio::task::spawn_blocking(move || state.supervisor.create_disk(req))
        .await
        .map_err(ApiError::bad_request)?
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn inspect_disk(
    State(state): State<AppState>,
    Json(req): Json<InspectDiskPayload>,
) -> Result<Json<DiskInfo>, ApiError> {
    tokio::task::spawn_blocking(move || state.supervisor.inspect_disk(&req.path))
        .await
        .map_err(ApiError::bad_request)?
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn list_vms(State(state): State<AppState>) -> Json<Vec<VmSummary>> {
    Json(state.supervisor.list_vms())
}

async fn create_vm(
    State(state): State<AppState>,
    Json(req): Json<CreateVmPayload>,
) -> Result<(StatusCode, Json<VmSummary>), ApiError> {
    let summary = state
        .supervisor
        .create_vm(req)
        .map_err(ApiError::map_anyhow)?;
    Ok((StatusCode::CREATED, Json(summary)))
}

async fn get_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .get_vm_with_live_properties(&id)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn update_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateVmPayload>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .update_vm(&id, req)
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

#[derive(Deserialize, Default)]
struct DeleteVmQuery {
    #[serde(default)]
    delete_disks: bool,
}

async fn delete_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<DeleteVmQuery>,
) -> Result<StatusCode, ApiError> {
    state
        .supervisor
        .delete_vm(&id, query.delete_disks)
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(ApiError::map_anyhow)
}

async fn start_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .start_vm(&id)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn stop_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .stop_vm(&id)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn shutdown_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
    payload: Option<Json<ShutdownVmPayload>>,
) -> Result<Json<VmSummary>, ApiError> {
    let req = payload.map(|p| p.0).unwrap_or_default();
    state
        .supervisor
        .shutdown_vm(&id, req)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn pause_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .pause_vm(&id)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn resume_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .resume_vm(&id)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn reset_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .reset_vm(&id)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn attach_disk(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AttachDiskPayload>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .attach_disk(&id, req)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn detach_disk(
    State(state): State<AppState>,
    Path((id, lun)): Path<(String, u32)>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .detach_disk(&id, lun)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn mount_iso(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<MountIsoPayload>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .mount_iso(&id, req)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn eject_iso_default(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .eject_iso(&id, None)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn eject_iso_lun(
    State(state): State<AppState>,
    Path((id, lun)): Path<(String, u32)>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .eject_iso(&id, Some(lun))
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn add_port_forward(
    State(state): State<AppState>,
    Path((id, nic_id)): Path<(String, Uuid)>,
    Json(req): Json<PortForwardPayload>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .add_port_forward(&id, nic_id, req.port)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn remove_port_forward(
    State(state): State<AppState>,
    Path((id, nic_id)): Path<(String, Uuid)>,
    Json(req): Json<PortForwardPayload>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .remove_port_forward(&id, nic_id, req.port)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default = "default_tail")]
    tail: usize,
}

fn default_tail() -> usize {
    200
}

#[derive(Serialize)]
struct LogResponse {
    content: String,
}

async fn vm_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<LogQuery>,
) -> Result<Json<LogResponse>, ApiError> {
    let content = state
        .supervisor
        .vm_logs(&id, q.tail)
        .map_err(ApiError::map_anyhow)?;
    Ok(Json(LogResponse { content }))
}

#[cfg(test)]
mod tests {
    use crate::TestDaemonHandle;
    use openvmms_core::models::CreateDiskPayload;
    use openvmms_core::models::CreateVmPayload;
    use openvmms_core::models::DiskFormat;
    use openvmms_core::models::DiskKind;
    use openvmms_core::models::IpProto;
    use openvmms_core::models::MountIsoPayload;
    use openvmms_core::models::NicBackendConfig;
    use openvmms_core::models::PortForward;
    use openvmms_core::models::PortForwardPayload;
    use openvmms_core::models::UpdateVmPayload;
    use openvmms_core::models::VmPreset;
    use openvmms_core::models::VmStatus;
    use openvmms_core::models::VmSummary;
    use std::fs;

    #[tokio::test]
    async fn daemon_rest_api_crud_disk_iso_port_forward_and_auth() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();

        let client = reqwest::Client::new();

        // Health is unauthenticated.
        let health = client
            .get(format!("{}/api/v1/health", daemon.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), 200);

        // Protected route rejects missing token.
        let unauth = client
            .get(format!("{}/api/v1/vms", daemon.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), 401);

        // Create a raw disk.
        let disk_path = dir.path().join("win11.img");
        let disk_resp = client
            .post(format!("{}/api/v1/disks/create", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateDiskPayload {
                path: disk_path.clone(),
                size_bytes: 32 * 1024 * 1024,
                format: DiskFormat::Raw,
                fixed: false,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(disk_resp.status(), 200);

        // Create a dummy ISO file.
        let iso_path = dir.path().join("installer.iso");
        fs::write(&iso_path, b"ISO9660_TEST").unwrap();

        // Create VM.
        let create_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "Dev-Win11".into(),
                description: "Test VM".into(),
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
                    path: dir.path().join("os.img"),
                    size_bytes: 16 * 1024 * 1024,
                    format: DiskFormat::Raw,
                    fixed: false,
                }),
                iso_path: Some(iso_path.clone()),
                nics: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(create_resp.status(), 201);
        let vm: VmSummary = create_resp.json().await.unwrap();
        assert_eq!(vm.runtime.status, VmStatus::Off);
        assert_eq!(vm.config.disks.len(), 2);
        assert_eq!(vm.config.disks[1].kind, DiskKind::Dvd);

        // Update VM.
        let update_resp = client
            .put(format!("{}/api/v1/vms/Dev-Win11", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&UpdateVmPayload {
                vcpus: Some(6),
                memory_mb: Some(12288),
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(update_resp.status(), 200);
        let vm: VmSummary = update_resp.json().await.unwrap();
        assert_eq!(vm.config.vcpus, 6);
        assert_eq!(vm.config.memory_mb, 12288);

        // Eject and re-mount ISO.
        let eject_resp = client
            .delete(format!("{}/api/v1/vms/Dev-Win11/iso", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(eject_resp.status(), 200);
        let vm: VmSummary = eject_resp.json().await.unwrap();
        assert_eq!(vm.config.disks.len(), 1);

        let mount_resp = client
            .post(format!("{}/api/v1/vms/Dev-Win11/iso", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&MountIsoPayload {
                path: iso_path,
                lun: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(mount_resp.status(), 200);
        let vm: VmSummary = mount_resp.json().await.unwrap();
        assert_eq!(vm.config.disks.len(), 2);

        // Add & remove port forward on NIC 0.
        let nic_id = vm.config.nics[0].id;
        let pf = PortForward {
            protocol: IpProto::Tcp,
            host_addr: "127.0.0.1".into(),
            host_port: 3389,
            guest_port: 3389,
        };
        let pf_add = client
            .post(format!(
                "{}/api/v1/vms/Dev-Win11/nics/{nic_id}/ports",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .json(&PortForwardPayload { port: pf.clone() })
            .send()
            .await
            .unwrap();
        assert_eq!(pf_add.status(), 200);
        let vm: VmSummary = pf_add.json().await.unwrap();
        match &vm.config.nics[0].backend {
            NicBackendConfig::Consomme { port_forwards, .. } => assert_eq!(port_forwards.len(), 1),
            other => panic!("unexpected nic backend: {other:?}"),
        }

        let pf_del = client
            .delete(format!(
                "{}/api/v1/vms/Dev-Win11/nics/{nic_id}/ports",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .json(&PortForwardPayload { port: pf })
            .send()
            .await
            .unwrap();
        assert_eq!(pf_del.status(), 200);

        // Delete VM.
        let del = client
            .delete(format!(
                "{}/api/v1/vms/Dev-Win11?delete_disks=true",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(del.status(), 204);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn real_openvmm_ttrpc_handshake_when_binary_present() {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir.join("../../..");
        let bin_name = if cfg!(windows) { "openvmm.exe" } else { "openvmm" };
        let openvmm_bin = [
            repo_root.join("openvmm/target/debug").join(bin_name),
            repo_root.join("openvmm/target/release").join(bin_name),
        ]
        .into_iter()
        .find(|p| p.is_file());

        let Some(openvmm_bin) = openvmm_bin else {
            if std::env::var_os("OPENVMM_REQUIRE_REAL_BINARY").is_some() {
                panic!(
                    "OPENVMM_REQUIRE_REAL_BINARY is set, but real openvmm binary was not found under {}",
                    repo_root.display()
                );
            }
            eprintln!("skipping real_openvmm_ttrpc_handshake: openvmm binary not built yet");
            return;
        };

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("openvmm.sock");
        let mut child = std::process::Command::new(&openvmm_bin)
            .arg("--rpc")
            .arg(format!("path={},transport=ttrpc", sock.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();

        let client = openvmms_core::ttrpc::TtrpcClient::new(&sock);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if sock.exists() && client.ping().await.is_ok() {
                break;
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("openvmm --rpc exited prematurely with {status}");
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for openvmm --rpc socket"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let props = client
            .properties_vm(openvmms_core::vmservice::PropertiesVmRequest { types: vec![] })
            .await
            .unwrap();
        assert_eq!(
            props.state,
            openvmms_core::vmservice::VmState::Uninitialized as i32
        );
        assert!(props.fastdesk_listen_addr.is_none());

        let caps = client.capabilities_vm().await.unwrap();
        assert!(!caps.supported_resources.is_empty());

        let _ = client.quit().await;
        let status = child.wait().unwrap();
        assert!(status.success(), "openvmm --rpc exited with {status}");
    }

    #[tokio::test]
    async fn delete_vm_without_delete_disks_preserves_default_and_vm_dir_disks() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = reqwest::Client::new();

        // Create VM with a default created disk (empty path -> <data_dir>/disks/...).
        let create_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "Preserve-Disks-VM".into(),
                description: String::new(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                nested_virt: None,
                uefi_firmware: None,
                secure_boot_enabled: None,
                secure_boot_template: None,
                tpm_enabled: None,
                fastdesk: None,
                disks: vec![],
                create_disk: Some(CreateDiskPayload {
                    path: std::path::PathBuf::new(),
                    size_bytes: 4 * 1024 * 1024,
                    format: DiskFormat::Raw,
                    fixed: false,
                }),
                iso_path: None,
                nics: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(create_resp.status(), 201);
        let vm: VmSummary = create_resp.json().await.unwrap();
        let default_disk = vm.config.disks[0].path.clone();
        assert!(default_disk.exists());
        assert!(default_disk.starts_with(dir.path().join("disks")));

        // Also place a disk explicitly inside vms/<id>/ and attach it.
        let inside_vm_dir_disk = dir
            .path()
            .join("vms")
            .join(vm.config.id.to_string())
            .join("extra.img");
        fs::write(&inside_vm_dir_disk, b"EXTRA_DISK").unwrap();
        let attach_resp = client
            .post(format!(
                "{}/api/v1/vms/{}/disks",
                daemon.base_url, vm.config.id
            ))
            .bearer_auth(&daemon.token)
            .json(&openvmms_core::models::AttachDiskPayload {
                path: inside_vm_dir_disk.clone(),
                lun: Some(1),
                read_only: false,
                format: Some(DiskFormat::Raw),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(attach_resp.status(), 200);

        // Delete WITHOUT delete_disks -> both disks must remain intact.
        let del = client
            .delete(format!("{}/api/v1/vms/{}", daemon.base_url, vm.config.id))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(del.status(), 204);
        assert!(
            default_disk.exists(),
            "default disk {} should be preserved when delete_disks=false",
            default_disk.display()
        );
        assert!(
            inside_vm_dir_disk.exists(),
            "disk inside vm dir {} should be preserved when delete_disks=false",
            inside_vm_dir_disk.display()
        );

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn mount_iso_rejects_non_dvd_lun_without_detaching_os_disk() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = reqwest::Client::new();

        let os_disk = dir.path().join("os.img");
        fs::write(&os_disk, b"OS_DISK").unwrap();
        let iso_path = dir.path().join("install.iso");
        fs::write(&iso_path, b"ISO_DATA").unwrap();

        let create_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "Lun-Guard-VM".into(),
                description: String::new(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                nested_virt: None,
                uefi_firmware: None,
                secure_boot_enabled: None,
                secure_boot_template: None,
                tpm_enabled: None,
                fastdesk: None,
                disks: vec![openvmms_core::models::DiskAttachment {
                    lun: 0,
                    kind: DiskKind::HardDisk,
                    path: os_disk.clone(),
                    read_only: false,
                    format: DiskFormat::Raw,
                }],
                create_disk: None,
                iso_path: None,
                nics: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(create_resp.status(), 201);

        // Attempt to mount ISO onto LUN 0 (occupied by HardDisk) -> must fail!
        let bad_mount = client
            .post(format!("{}/api/v1/vms/Lun-Guard-VM/iso", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&MountIsoPayload {
                path: iso_path.clone(),
                lun: Some(0),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(bad_mount.status(), 409);

        // Verify LUN 0 is still the OS HardDisk.
        let vm: VmSummary = client
            .get(format!("{}/api/v1/vms/Lun-Guard-VM", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(vm.config.disks.len(), 1);
        assert_eq!(vm.config.disks[0].lun, 0);
        assert_eq!(vm.config.disks[0].kind, DiskKind::HardDisk);
        assert_eq!(vm.config.disks[0].path, os_disk);

        // Mounting ISO with lun: None succeeds onto free LUN 1.
        let ok_mount = client
            .post(format!("{}/api/v1/vms/Lun-Guard-VM/iso", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&MountIsoPayload {
                path: iso_path,
                lun: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(ok_mount.status(), 200);
        let vm: VmSummary = ok_mount.json().await.unwrap();
        assert_eq!(vm.config.disks.len(), 2);
        assert_eq!(vm.config.disks[1].lun, 1);
        assert_eq!(vm.config.disks[1].kind, DiskKind::Dvd);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn sse_events_stream_closes_promptly_on_daemon_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = reqwest::Client::new();

        // Open a persistent SSE stream like openvmms-gui does.
        let mut sse_resp = client
            .get(format!(
                "{}/api/v1/events?token={}",
                daemon.base_url, daemon.token
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(sse_resp.status(), 200);

        // Request daemon shutdown via API while the SSE stream is open.
        let shutdown_resp = client
            .post(format!("{}/api/v1/daemon/shutdown", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(shutdown_resp.status(), 202);

        // Both the SSE stream and the server task must exit within 2 seconds.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut saw_stopping = false;
            while let Some(chunk) = sse_resp.chunk().await.unwrap() {
                let text = String::from_utf8_lossy(&chunk);
                if text.contains("daemon_stopping") {
                    saw_stopping = true;
                }
            }
            assert!(saw_stopping, "expected daemon_stopping SSE event before EOF");
            daemon.shutdown().await;
        })
        .await
        .expect("SSE stream and daemon did not shut down within 2s");
    }

    #[tokio::test]
    async fn updating_host_config_listen_addr_serves_new_port_on_same_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = reqwest::Client::new();

        // Create a VM on the original port so we can confirm state continuity on the new port.
        let create_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "PortSwitch-VM".into(),
                description: String::new(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
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
            .send()
            .await
            .unwrap();
        assert_eq!(create_resp.status(), 201);

        let mut cfg: openvmms_core::models::HostConfig = client
            .get(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        // Invalid listen address must fail without overwriting config.
        let mut bad_cfg = cfg.clone();
        bad_cfg.listen_addr = "not-a-socket-addr".into();
        let bad_resp = client
            .put(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&bad_cfg)
            .send()
            .await
            .unwrap();
        assert_eq!(bad_resp.status(), 400);

        // Bind a fresh ephemeral port on the same daemon.
        cfg.listen_addr = "127.0.0.1:0".into();
        let put_resp = client
            .put(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&cfg)
            .send()
            .await
            .unwrap();
        assert_eq!(put_resp.status(), 200);
        let updated: openvmms_core::models::HostConfig = put_resp.json().await.unwrap();
        let new_url = format!("http://{}", updated.listen_addr);
        assert_ne!(new_url, daemon.base_url);

        // ApiClient::from_data_dir resolves the persisted new listen_addr and talks to the SAME daemon.
        let data_dir = dir.path().to_path_buf();
        let (health_ok, vm_names): (bool, Vec<String>) = tokio::task::spawn_blocking(move || {
            let api = openvmms_core::api_client::ApiClient::from_data_dir(Some(&data_dir), None)
                .unwrap();
            assert_eq!(api.base_url(), new_url);
            api.health().unwrap();
            let vms = api.list_vms().unwrap();
            (
                true,
                vms.into_iter().map(|v| v.config.name).collect(),
            )
        })
        .await
        .unwrap();
        assert!(health_ok);
        assert_eq!(vm_names, vec!["PortSwitch-VM".to_string()]);

        // Previous listener must stop accepting new connections once the port changes.
        let old_addr: std::net::SocketAddr = daemon
            .base_url
            .trim_start_matches("http://")
            .parse()
            .unwrap();
        let mut old_closed = false;
        for _ in 0..40 {
            if tokio::net::TcpStream::connect(old_addr).await.is_err() {
                old_closed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            old_closed,
            "old HTTP listener on {old_addr} kept accepting connections after port change"
        );

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn attach_disk_while_vm_starting_is_rejected_with_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = reqwest::Client::new();

        let create_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "Starting-VM".into(),
                description: String::new(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
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
            .send()
            .await
            .unwrap();
        assert_eq!(create_resp.status(), 201);
        let created: VmSummary = create_resp.json().await.unwrap();

        daemon
            .supervisor
            .set_vm_status_for_test(created.config.id, openvmms_core::models::VmStatus::Starting);

        let disk_path = dir.path().join("extra.img");
        fs::write(&disk_path, b"DISK").unwrap();
        let attach_resp = client
            .post(format!("{}/api/v1/vms/Starting-VM/disks", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&openvmms_core::models::AttachDiskPayload {
                path: disk_path,
                lun: Some(0),
                read_only: false,
                format: Some(DiskFormat::Raw),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(attach_resp.status(), 409);

        let iso_path = dir.path().join("boot.iso");
        fs::write(&iso_path, b"ISO").unwrap();
        let iso_resp = client
            .post(format!("{}/api/v1/vms/Starting-VM/iso", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&MountIsoPayload {
                path: iso_path,
                lun: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(iso_resp.status(), 409);

        let vm: VmSummary = client
            .get(format!("{}/api/v1/vms/Starting-VM", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(vm.config.disks.is_empty());

        daemon
            .supervisor
            .set_vm_status_for_test(created.config.id, openvmms_core::models::VmStatus::Off);
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn delayed_pause_resume_result_does_not_clobber_stop_or_restart() {
        use openvmms_core::models::VmStatus;
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let created = daemon
            .supervisor
            .create_vm(CreateVmPayload {
                name: "Race-VM".into(),
                description: String::new(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
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
        let id = created.config.id;
        let sup = &daemon.supervisor;
        sup.set_vm_status_for_test(id, VmStatus::Running);

        // Pause RPC issued while Running, then a stop begins before the reply lands.
        let rpc_gen = sup.generation_for_test(id);
        sup.advance_lifecycle_for_test(id, VmStatus::Stopping);
        assert!(sup.commit_power_transition(id, rpc_gen, "pause", VmStatus::Paused).is_err());
        assert_eq!(sup.get_vm("Race-VM").unwrap().runtime.status, VmStatus::Stopping);
        assert!(sup.commit_power_transition(id, rpc_gen, "resume", VmStatus::Running).is_err());
        assert_eq!(sup.get_vm("Race-VM").unwrap().runtime.status, VmStatus::Stopping);

        // Replacement VM is now Running; the stale reply must not pause it either.
        sup.advance_lifecycle_for_test(id, VmStatus::Running);
        assert!(sup.commit_power_transition(id, rpc_gen, "pause", VmStatus::Paused).is_err());
        assert_eq!(sup.get_vm("Race-VM").unwrap().runtime.status, VmStatus::Running);

        // A reply from the current lifecycle generation still applies.
        let cur = sup.generation_for_test(id);
        sup.commit_power_transition(id, cur, "pause", VmStatus::Paused).unwrap();
        assert_eq!(sup.get_vm("Race-VM").unwrap().runtime.status, VmStatus::Paused);

        sup.set_vm_status_for_test(id, VmStatus::Off);
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn update_vm_clears_firmware_on_null_and_preserves_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = reqwest::Client::new();

        let fw_path = dir.path().join("custom-msvm.fd");
        let create_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "Firmware-VM".into(),
                description: String::new(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                nested_virt: None,
                uefi_firmware: Some(fw_path.clone()),
                secure_boot_enabled: None,
                secure_boot_template: None,
                tpm_enabled: None,
                fastdesk: None,
                disks: vec![],
                create_disk: None,
                iso_path: None,
                nics: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(create_resp.status(), 201);
        let created: VmSummary = create_resp.json().await.unwrap();
        assert_eq!(created.config.uefi_firmware.as_ref(), Some(&fw_path));

        // Omitting uefi_firmware must preserve the existing override.
        let keep_resp = client
            .put(format!("{}/api/v1/vms/Firmware-VM", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&serde_json::json!({ "description": "updated" }))
            .send()
            .await
            .unwrap();
        assert_eq!(keep_resp.status(), 200);
        let kept: VmSummary = keep_resp.json().await.unwrap();
        assert_eq!(kept.config.description, "updated");
        assert_eq!(kept.config.uefi_firmware.as_ref(), Some(&fw_path));

        // Explicit JSON null must clear the override back to None.
        let clear_resp = client
            .put(format!("{}/api/v1/vms/Firmware-VM", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&serde_json::json!({ "uefi_firmware": null }))
            .send()
            .await
            .unwrap();
        assert_eq!(clear_resp.status(), 200);
        let cleared: VmSummary = clear_resp.json().await.unwrap();
        assert_eq!(cleared.config.uefi_firmware, None);

        daemon.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_vm_during_startup_then_restarting_preserves_replacement_vm() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = reqwest::Client::new();

        let state_dir = dir.path().join("stub_state");
        fs::create_dir_all(&state_dir).unwrap();
        let stub_path = dir.path().join("openvmm_stub.py");
        let stub_script = format!(
            r#"#!/usr/bin/env python3
import os, socket, struct, sys, threading, time

STATE_DIR = {state_dir:?}
sock_path = None
args = sys.argv[1:]
for i, a in enumerate(args):
    if a == "--rpc" and i + 1 < len(args):
        for part in args[i + 1].split(","):
            if part.startswith("path="):
                sock_path = part[5:]

count_file = os.path.join(STATE_DIR, "launch_count")
try:
    with open(count_file, "r") as f:
        launch = int(f.read().strip()) + 1
except FileNotFoundError:
    launch = 1
with open(count_file, "w") as f:
    f.write(str(launch))

if os.path.exists(sock_path):
    os.unlink(sock_path)

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sock_path)
srv.listen(32)
stop_evt = threading.Event()

def send_ok(conn, stream_id):
    env = b"\x12\x00"
    hdr = struct.pack(">IIBB", len(env), stream_id, 2, 0)
    conn.sendall(hdr + env)

def send_err(conn, stream_id, msg):
    msg_b = msg.encode()
    st = b"\x08\x0d\x12" + bytes([len(msg_b)]) + msg_b
    env = b"\x0a" + bytes([len(st)]) + st
    hdr = struct.pack(">IIBB", len(env), stream_id, 2, 0)
    conn.sendall(hdr + env)

def handle(conn):
    try:
        hdr = conn.recv(10)
        if len(hdr) < 10:
            return
        length, stream_id, _, _ = struct.unpack(">IIBB", hdr)
        data = b""
        while len(data) < length:
            chunk = conn.recv(length - len(data))
            if not chunk:
                return
            data += chunk
        if b"CreateVM" in data:
            if launch == 1:
                open(os.path.join(STATE_DIR, "first_create_entered"), "w").close()
                time.sleep(0.7)
                send_err(conn, stream_id, "delayed failure from cancelled startup")
            else:
                send_ok(conn, stream_id)
        elif b"WaitVM" in data:
            stop_evt.wait(timeout=15)
            send_ok(conn, stream_id)
        elif b"TeardownVM" in data or b"Quit" in data:
            stop_evt.set()
            send_ok(conn, stream_id)
        else:
            send_ok(conn, stream_id)
    except Exception:
        pass
    finally:
        conn.close()

while not stop_evt.is_set():
    srv.settimeout(0.1)
    try:
        conn, _ = srv.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()
    except socket.timeout:
        continue
    except OSError:
        break
"#,
            state_dir = state_dir.to_string_lossy().into_owned(),
        );
        fs::write(&stub_path, stub_script).unwrap();
        fs::set_permissions(&stub_path, fs::Permissions::from_mode(0o755)).unwrap();

        let fw_path = dir.path().join("MSVM.fd");
        fs::write(&fw_path, b"FAKE_UEFI").unwrap();

        let mut cfg: openvmms_core::models::HostConfig = client
            .get(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        cfg.openvmm_bin = Some(stub_path);
        cfg.uefi_firmware_path = Some(fw_path);
        let cfg_resp = client
            .put(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&cfg)
            .send()
            .await
            .unwrap();
        assert_eq!(cfg_resp.status(), 200);

        let create_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "Race-Restart-VM".into(),
                description: String::new(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                nested_virt: None,
                uefi_firmware: None,
                secure_boot_enabled: None,
                secure_boot_template: None,
                tpm_enabled: None,
                fastdesk: Some(openvmms_core::models::FastDeskSettings {
                    enabled: false,
                    ..Default::default()
                }),
                disks: vec![],
                create_disk: None,
                iso_path: None,
                nics: Some(vec![]),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(create_resp.status(), 201);

        // Start #1 blocks inside CreateVM RPC in the stub.
        let base_url = daemon.base_url.clone();
        let token = daemon.token.clone();
        let start1 = tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{base_url}/api/v1/vms/Race-Restart-VM/start"))
                .bearer_auth(token)
                .send()
                .await
        });

        let sentinel = state_dir.join("first_create_entered");
        let wait_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !sentinel.exists() {
            assert!(
                tokio::time::Instant::now() < wait_deadline,
                "stub never entered first CreateVM"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // Interrupt startup #1 with stop_vm.
        let stop_resp = client
            .post(format!(
                "{}/api/v1/vms/Race-Restart-VM/stop",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(stop_resp.status(), 200);

        // Immediately start #2 (replacement process).
        let start2_resp = client
            .post(format!(
                "{}/api/v1/vms/Race-Restart-VM/start",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(start2_resp.status(), 200);
        let started2: VmSummary = start2_resp.json().await.unwrap();
        assert_eq!(
            started2.runtime.status,
            openvmms_core::models::VmStatus::Running
        );
        let pid2 = started2.runtime.pid.expect("replacement VM must have PID");

        // Wait past launch #1's sleep and ensure start1 finished without harming replacement VM #2.
        let _ = start1.await;
        tokio::time::sleep(std::time::Duration::from_millis(850)).await;

        let check: VmSummary = client
            .get(format!("{}/api/v1/vms/Race-Restart-VM", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            check.runtime.status,
            openvmms_core::models::VmStatus::Running
        );
        assert_eq!(check.runtime.pid, Some(pid2));

        let final_stop = client
            .post(format!(
                "{}/api/v1/vms/Race-Restart-VM/stop",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(final_stop.status(), 200);

        daemon.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_stop_requests_wait_for_cleanup_and_preserve_restarted_vm() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = reqwest::Client::new();

        let state_dir = dir.path().join("stub_state");
        fs::create_dir_all(&state_dir).unwrap();
        let stub_path = dir.path().join("openvmm_stop_stub.py");
        let stub_script = format!(
            r#"#!/usr/bin/env python3
import os, socket, struct, sys, threading, time

STATE_DIR = {state_dir:?}
sock_path = None
args = sys.argv[1:]
for i, a in enumerate(args):
    if a == "--rpc" and i + 1 < len(args):
        for part in args[i + 1].split(","):
            if part.startswith("path="):
                sock_path = part[5:]

count_file = os.path.join(STATE_DIR, "launch_count")
try:
    with open(count_file, "r") as f:
        launch = int(f.read().strip()) + 1
except FileNotFoundError:
    launch = 1
with open(count_file, "w") as f:
    f.write(str(launch))

if os.path.exists(sock_path):
    os.unlink(sock_path)

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sock_path)
srv.listen(32)
stop_evt = threading.Event()

def send_ok(conn, stream_id):
    env = b"\x12\x00"
    hdr = struct.pack(">IIBB", len(env), stream_id, 2, 0)
    conn.sendall(hdr + env)

def handle(conn):
    try:
        hdr = conn.recv(10)
        if len(hdr) < 10:
            return
        length, stream_id, _, _ = struct.unpack(">IIBB", hdr)
        data = b""
        while len(data) < length:
            chunk = conn.recv(length - len(data))
            if not chunk:
                return
            data += chunk
        if b"WaitVM" in data:
            stop_evt.wait(timeout=15)
            send_ok(conn, stream_id)
        elif b"TeardownVM" in data:
            if launch == 1:
                open(os.path.join(STATE_DIR, "teardown_entered"), "w").close()
                time.sleep(0.6)
                open(os.path.join(STATE_DIR, "teardown_finished"), "w").close()
            stop_evt.set()
            send_ok(conn, stream_id)
        elif b"Quit" in data:
            if launch == 2:
                open(os.path.join(STATE_DIR, "launch2_quit"), "w").close()
            stop_evt.set()
            send_ok(conn, stream_id)
        else:
            send_ok(conn, stream_id)
    except Exception:
        pass
    finally:
        conn.close()

while not stop_evt.is_set():
    srv.settimeout(0.1)
    try:
        conn, _ = srv.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()
    except socket.timeout:
        continue
    except OSError:
        break
"#,
            state_dir = state_dir.to_string_lossy().into_owned(),
        );
        fs::write(&stub_path, stub_script).unwrap();
        fs::set_permissions(&stub_path, fs::Permissions::from_mode(0o755)).unwrap();

        let fw_path = dir.path().join("MSVM.fd");
        fs::write(&fw_path, b"FAKE_UEFI").unwrap();

        let mut cfg: openvmms_core::models::HostConfig = client
            .get(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        cfg.openvmm_bin = Some(stub_path);
        cfg.uefi_firmware_path = Some(fw_path);
        assert_eq!(
            client
                .put(format!("{}/api/v1/host/config", daemon.base_url))
                .bearer_auth(&daemon.token)
                .json(&cfg)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );

        let create_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "ConcurrentStop-VM".into(),
                description: String::new(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                nested_virt: None,
                uefi_firmware: None,
                secure_boot_enabled: None,
                secure_boot_template: None,
                tpm_enabled: None,
                fastdesk: Some(openvmms_core::models::FastDeskSettings {
                    enabled: false,
                    ..Default::default()
                }),
                disks: vec![],
                create_disk: None,
                iso_path: None,
                nics: Some(vec![]),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(create_resp.status(), 201);

        let start1_resp = client
            .post(format!(
                "{}/api/v1/vms/ConcurrentStop-VM/start",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(start1_resp.status(), 200);

        let base_url = daemon.base_url.clone();
        let token = daemon.token.clone();
        let stop1 = tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{base_url}/api/v1/vms/ConcurrentStop-VM/stop"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
        });

        let teardown_entered = state_dir.join("teardown_entered");
        let teardown_finished = state_dir.join("teardown_finished");
        let wait_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !teardown_entered.exists() {
            assert!(
                tokio::time::Instant::now() < wait_deadline,
                "stub never entered first TeardownVM"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // Starting while Stopping must be rejected with 409 Conflict.
        let early_start = client
            .post(format!(
                "{}/api/v1/vms/ConcurrentStop-VM/start",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(early_start.status(), 409);

        // Concurrent second stop must wait until the first stop's cleanup finishes.
        let stop2_resp = client
            .post(format!(
                "{}/api/v1/vms/ConcurrentStop-VM/stop",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(stop2_resp.status(), 200);
        let stop2_vm: VmSummary = stop2_resp.json().await.unwrap();
        assert_eq!(stop2_vm.runtime.status, VmStatus::Off);
        assert!(
            teardown_finished.exists(),
            "second stop returned before first stop finished TeardownVM"
        );

        let stop1_resp = stop1.await.unwrap();
        assert_eq!(stop1_resp.status(), 200);
        let stop1_vm: VmSummary = stop1_resp.json().await.unwrap();
        assert_eq!(stop1_vm.runtime.status, VmStatus::Off);

        // Immediately restart the VM and verify the replacement instance stays Running.
        let start2_resp = client
            .post(format!(
                "{}/api/v1/vms/ConcurrentStop-VM/start",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(start2_resp.status(), 200);
        let started2: VmSummary = start2_resp.json().await.unwrap();
        assert_eq!(started2.runtime.status, VmStatus::Running);
        let pid2 = started2.runtime.pid.expect("replacement VM must have PID");

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !state_dir.join("launch2_quit").exists(),
            "replacement VM received unexpected Quit from prior stop"
        );
        let check: VmSummary = client
            .get(format!("{}/api/v1/vms/ConcurrentStop-VM", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(check.runtime.status, VmStatus::Running);
        assert_eq!(check.runtime.pid, Some(pid2));

        let final_stop = client
            .post(format!(
                "{}/api/v1/vms/ConcurrentStop-VM/stop",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(final_stop.status(), 200);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn failed_create_vm_does_not_leave_created_disk_behind_and_retry_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = reqwest::Client::new();

        let disk_path = dir.path().join("retry-os.img");
        for bad_vcpus in [0, 512] {
            let bad_resp = client
                .post(format!("{}/api/v1/vms", daemon.base_url))
                .bearer_auth(&daemon.token)
                .json(&CreateVmPayload {
                    name: "Validate-First-VM".into(),
                    description: String::new(),
                    preset: VmPreset::Custom,
                    vcpus: Some(bad_vcpus),
                    memory_mb: Some(1024),
                    nested_virt: None,
                    uefi_firmware: None,
                    secure_boot_enabled: None,
                    secure_boot_template: None,
                    tpm_enabled: None,
                    fastdesk: None,
                    disks: vec![],
                    create_disk: Some(CreateDiskPayload {
                        path: disk_path.clone(),
                        size_bytes: 16 * 1024 * 1024,
                        format: DiskFormat::Raw,
                        fixed: false,
                    }),
                    iso_path: None,
                    nics: None,
                })
                .send()
                .await
                .unwrap();
            assert!(
                bad_resp.status().is_client_error(),
                "expected 4xx error for bad vcpus={bad_vcpus}, got {}",
                bad_resp.status()
            );
            let err_body = bad_resp.text().await.unwrap();
            assert!(
                err_body.contains("vcpus must be between 1 and 256"),
                "unexpected error body: {err_body}"
            );
            assert!(
                !disk_path.exists(),
                "failed create_vm with vcpus={bad_vcpus} left disk {} behind",
                disk_path.display()
            );
        }

        // Retrying with corrected vcpus using the same disk path must now succeed.
        let ok_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "Validate-First-VM".into(),
                description: String::new(),
                preset: VmPreset::Custom,
                vcpus: Some(4),
                memory_mb: Some(1024),
                nested_virt: None,
                uefi_firmware: None,
                secure_boot_enabled: None,
                secure_boot_template: None,
                tpm_enabled: None,
                fastdesk: None,
                disks: vec![],
                create_disk: Some(CreateDiskPayload {
                    path: disk_path.clone(),
                    size_bytes: 16 * 1024 * 1024,
                    format: DiskFormat::Raw,
                    fixed: false,
                }),
                iso_path: None,
                nics: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(ok_resp.status(), 201);
        let vm: VmSummary = ok_resp.json().await.unwrap();
        assert_eq!(vm.config.disks.len(), 1);
        assert_eq!(vm.config.disks[0].path, disk_path);
        assert!(disk_path.exists());

        daemon.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delayed_pause_and_resume_rpcs_do_not_break_concurrent_stop_and_restart() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = reqwest::Client::new();

        let state_dir = dir.path().join("stub_state");
        fs::create_dir_all(&state_dir).unwrap();
        let stub_path = dir.path().join("openvmm_pause_resume_stub.py");
        let stub_script = format!(
            r#"#!/usr/bin/env python3
import os, socket, struct, sys, threading, time

STATE_DIR = {state_dir:?}
sock_path = None
args = sys.argv[1:]
for i, a in enumerate(args):
    if a == "--rpc" and i + 1 < len(args):
        for part in args[i + 1].split(","):
            if part.startswith("path="):
                sock_path = part[5:]

count_file = os.path.join(STATE_DIR, "launch_count")
try:
    with open(count_file, "r") as f:
        launch = int(f.read().strip()) + 1
except FileNotFoundError:
    launch = 1
with open(count_file, "w") as f:
    f.write(str(launch))

if os.path.exists(sock_path):
    os.unlink(sock_path)

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sock_path)
srv.listen(32)
stop_evt = threading.Event()

def send_ok(conn, stream_id):
    env = b"\x12\x00"
    hdr = struct.pack(">IIBB", len(env), stream_id, 2, 0)
    conn.sendall(hdr + env)

def handle(conn):
    try:
        hdr = conn.recv(10)
        if len(hdr) < 10:
            return
        length, stream_id, _, _ = struct.unpack(">IIBB", hdr)
        data = b""
        while len(data) < length:
            chunk = conn.recv(length - len(data))
            if not chunk:
                return
            data += chunk
        if b"WaitVM" in data:
            stop_evt.wait(timeout=15)
            send_ok(conn, stream_id)
        elif b"PauseVM" in data:
            if launch == 1:
                open(os.path.join(STATE_DIR, "pause_entered"), "w").close()
                time.sleep(0.35)
            send_ok(conn, stream_id)
        elif b"ResumeVM" in data:
            if launch == 2:
                open(os.path.join(STATE_DIR, "resume_entered"), "w").close()
                time.sleep(0.35)
            send_ok(conn, stream_id)
        elif b"TeardownVM" in data:
            if launch == 1:
                open(os.path.join(STATE_DIR, "teardown_entered_1"), "w").close()
                time.sleep(0.75)
            elif launch == 2:
                open(os.path.join(STATE_DIR, "teardown_entered_2"), "w").close()
                time.sleep(0.75)
            stop_evt.set()
            send_ok(conn, stream_id)
        elif b"Quit" in data:
            if launch == 2:
                open(os.path.join(STATE_DIR, "launch2_quit"), "w").close()
            elif launch == 3:
                open(os.path.join(STATE_DIR, "launch3_quit"), "w").close()
            stop_evt.set()
            send_ok(conn, stream_id)
        else:
            send_ok(conn, stream_id)
    except Exception:
        pass
    finally:
        conn.close()

while not stop_evt.is_set():
    srv.settimeout(0.1)
    try:
        conn, _ = srv.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()
    except socket.timeout:
        continue
    except OSError:
        break
"#,
            state_dir = state_dir.to_string_lossy().into_owned(),
        );
        fs::write(&stub_path, stub_script).unwrap();
        fs::set_permissions(&stub_path, fs::Permissions::from_mode(0o755)).unwrap();

        let fw_path = dir.path().join("MSVM.fd");
        fs::write(&fw_path, b"FAKE_UEFI").unwrap();

        let mut cfg: openvmms_core::models::HostConfig = client
            .get(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        cfg.openvmm_bin = Some(stub_path);
        cfg.uefi_firmware_path = Some(fw_path);
        assert_eq!(
            client
                .put(format!("{}/api/v1/host/config", daemon.base_url))
                .bearer_auth(&daemon.token)
                .json(&cfg)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );

        let create_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "PauseStop-VM".into(),
                description: String::new(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                nested_virt: None,
                uefi_firmware: None,
                secure_boot_enabled: None,
                secure_boot_template: None,
                tpm_enabled: None,
                fastdesk: Some(openvmms_core::models::FastDeskSettings {
                    enabled: false,
                    ..Default::default()
                }),
                disks: vec![],
                create_disk: None,
                iso_path: None,
                nics: Some(vec![]),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(create_resp.status(), 201);

        // Phase 1: delayed PauseVM vs concurrent StopVM.
        let start1_resp = client
            .post(format!("{}/api/v1/vms/PauseStop-VM/start", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(start1_resp.status(), 200);

        let base_url = daemon.base_url.clone();
        let token = daemon.token.clone();
        let pause_task = tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{base_url}/api/v1/vms/PauseStop-VM/pause"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
        });

        let wait_for = |sentinel: std::path::PathBuf| async move {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            while !sentinel.exists() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "stub never reached {}",
                    sentinel.display()
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };

        wait_for(state_dir.join("pause_entered")).await;

        let base_url = daemon.base_url.clone();
        let token = daemon.token.clone();
        let stop1_task = tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{base_url}/api/v1/vms/PauseStop-VM/stop"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
        });

        wait_for(state_dir.join("teardown_entered_1")).await;

        let pause_resp = pause_task.await.unwrap();
        assert_eq!(pause_resp.status(), 409);

        let during_stop1: VmSummary = client
            .get(format!("{}/api/v1/vms/PauseStop-VM", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(during_stop1.runtime.status, VmStatus::Stopping);

        let early_start = client
            .post(format!("{}/api/v1/vms/PauseStop-VM/start", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(early_start.status(), 409);

        let stop1_resp = stop1_task.await.unwrap();
        assert_eq!(stop1_resp.status(), 200);
        let stop1_vm: VmSummary = stop1_resp.json().await.unwrap();
        assert_eq!(stop1_vm.runtime.status, VmStatus::Off);

        // Start launch 2 and verify launch 1's stop did not harm it.
        let start2_resp = client
            .post(format!("{}/api/v1/vms/PauseStop-VM/start", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(start2_resp.status(), 200);
        assert!(!state_dir.join("launch2_quit").exists());

        // Phase 2: pause launch 2 cleanly, then delayed ResumeVM vs concurrent StopVM.
        let pause2_resp = client
            .post(format!("{}/api/v1/vms/PauseStop-VM/pause", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(pause2_resp.status(), 200);
        let paused2: VmSummary = pause2_resp.json().await.unwrap();
        assert_eq!(paused2.runtime.status, VmStatus::Paused);

        let base_url = daemon.base_url.clone();
        let token = daemon.token.clone();
        let resume_task = tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{base_url}/api/v1/vms/PauseStop-VM/resume"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
        });

        wait_for(state_dir.join("resume_entered")).await;

        let base_url = daemon.base_url.clone();
        let token = daemon.token.clone();
        let stop2_task = tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{base_url}/api/v1/vms/PauseStop-VM/stop"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
        });

        wait_for(state_dir.join("teardown_entered_2")).await;

        let resume_resp = resume_task.await.unwrap();
        assert_eq!(resume_resp.status(), 409);

        let during_stop2: VmSummary = client
            .get(format!("{}/api/v1/vms/PauseStop-VM", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(during_stop2.runtime.status, VmStatus::Stopping);

        let stop2_resp = stop2_task.await.unwrap();
        assert_eq!(stop2_resp.status(), 200);
        let stop2_vm: VmSummary = stop2_resp.json().await.unwrap();
        assert_eq!(stop2_vm.runtime.status, VmStatus::Off);

        // Start launch 3 and verify it remains Running with its socket intact.
        let start3_resp = client
            .post(format!("{}/api/v1/vms/PauseStop-VM/start", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(start3_resp.status(), 200);
        let started3: VmSummary = start3_resp.json().await.unwrap();
        assert_eq!(started3.runtime.status, VmStatus::Running);
        let pid3 = started3.runtime.pid.expect("replacement VM must have PID");

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !state_dir.join("launch3_quit").exists(),
            "replacement VM received unexpected Quit from prior stop"
        );
        let check: VmSummary = client
            .get(format!("{}/api/v1/vms/PauseStop-VM", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(check.runtime.status, VmStatus::Running);
        assert_eq!(check.runtime.pid, Some(pid3));
        assert!(
            check
                .runtime
                .rpc_socket_path
                .as_ref()
                .is_some_and(|p| p.exists()),
            "replacement VM RPC socket should remain intact"
        );

        let final_stop = client
            .post(format!("{}/api/v1/vms/PauseStop-VM/stop", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(final_stop.status(), 200);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn standalone_create_disk_rejects_incompatible_extension() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = reqwest::Client::new();

        for (name, format) in [
            ("disk.vhdx", DiskFormat::Raw),
            ("disk.vhd", DiskFormat::Vhdx),
            ("disk.iso", DiskFormat::Vhdx),
            ("disk.vmdk", DiskFormat::Raw),
        ] {
            let path = dir.path().join(name);
            let resp = client
                .post(format!("{}/api/v1/disks/create", daemon.base_url))
                .bearer_auth(&daemon.token)
                .json(&CreateDiskPayload {
                    path: path.clone(),
                    size_bytes: 16 * 1024 * 1024,
                    format,
                    fixed: false,
                })
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 400, "{name} as {format:?} should fail with 400");
            assert!(!path.exists(), "{name} must not be created");
        }

        let valid_path = dir.path().join("disk.img");
        let ok_resp = client
            .post(format!("{}/api/v1/disks/create", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateDiskPayload {
                path: valid_path.clone(),
                size_bytes: 16 * 1024 * 1024,
                format: DiskFormat::Raw,
                fixed: false,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(ok_resp.status(), 200);
        assert!(valid_path.exists());

        daemon.shutdown().await;
    }
}
