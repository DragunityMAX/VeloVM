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
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
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

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.supervisor.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(event) => serde_json::to_string(&event)
            .ok()
            .map(|json| Ok(Event::default().data(json))),
        Err(_) => None,
    });
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
) -> Result<Json<HostConfig>, ApiError> {
    state
        .supervisor
        .update_host_config(cfg)
        .await
        .map(Json)
        .map_err(ApiError::map_anyhow)
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

        client.quit().await.unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "openvmm --rpc exited with {status}");
    }
}
