use crate::supervisor::ApiErrorKind;
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
use velovm_core::models::ApiErrorBody;
use velovm_core::models::AttachDiskPayload;
use velovm_core::models::CreateDiskPayload;
use velovm_core::models::CreateSwitchPayload;
use velovm_core::models::CreateVmPayload;
use velovm_core::models::DaemonEvent;
use velovm_core::models::DiskInfo;
use velovm_core::models::HostConfig;
use velovm_core::models::HostConfigPatch;
use velovm_core::models::HostInfo;
use velovm_core::models::HostNetAdapterInfo;
use velovm_core::models::InspectDiskPayload;
use velovm_core::models::MountIsoPayload;
use velovm_core::models::PortForwardPayload;
use velovm_core::models::ShutdownVmPayload;
use velovm_core::models::UpdateVmPayload;
use velovm_core::models::VirtualSwitchInfo;
use velovm_core::models::VmSummary;
use serde::Deserialize;
use serde::Serialize;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use uuid::Uuid;

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

fn spawn_cancel_on_shutdown(sup: &Arc<Supervisor>, cancel_flag: &Arc<AtomicBool>) -> AbortOnDrop {
    let sup = Arc::clone(sup);
    let cancel_flag = Arc::clone(cancel_flag);
    let handle = tokio::spawn(async move {
        sup.wait_for_shutdown_signal().await;
        cancel_flag.store(true, Ordering::Release);
    });
    AbortOnDrop(Some(handle))
}

#[derive(Clone)]
pub struct AppState {
    pub supervisor: Arc<Supervisor>,
    pub token: String,
    pub bound_addr: SocketAddr,
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn map_anyhow(err: anyhow::Error) -> Self {
        let status = match err
            .chain()
            .find_map(|cause| cause.downcast_ref::<ApiErrorKind>())
        {
            Some(ApiErrorKind::NotFound(_)) => StatusCode::NOT_FOUND,
            Some(ApiErrorKind::Conflict(_)) => StatusCode::CONFLICT,
            Some(ApiErrorKind::BadRequest(_)) => StatusCode::BAD_REQUEST,
            Some(ApiErrorKind::Internal(_)) | None => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: format!("{err:#}"),
        }
    }

    fn internal(err: impl ToString) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: err.to_string(),
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

pub fn build_router(supervisor: Arc<Supervisor>, token: String, bound_addr: SocketAddr) -> Router {
    let state = AppState {
        supervisor,
        token,
        bound_addr,
    };

    let protected = Router::new()
        .route("/api/v1/events", get(events_handler))
        .route("/api/v1/host/info", get(host_info))
        .route(
            "/api/v1/host/config",
            get(get_host_config)
                .put(put_host_config)
                .patch(patch_host_config),
        )
        .route(
            "/api/v1/host/switches",
            get(list_switches).post(create_switch),
        )
        .route("/api/v1/host/switches/{id}", delete(delete_switch))
        .route("/api/v1/host/net-adapters", get(list_net_adapters))
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

    if header_token.is_some_and(|t| constant_time_eq(t.as_bytes(), state.token.as_bytes())) {
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
        .map(|c| velovm_core::models::compute_health_auth_proof(&state.token, state.bound_addr, c));
    Json(HealthResponse {
        status: "ok",
        service: "velovmd",
        auth_proof,
    })
}

fn drain_for_daemon_stopping(
    rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
) -> Option<Event> {
    use tokio::sync::broadcast::error::TryRecvError;

    loop {
        match rx.try_recv() {
            Ok(DaemonEvent::DaemonStopping) => break,
            Ok(_) | Err(TryRecvError::Lagged(_)) => continue,
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }
    serde_json::to_string(&DaemonEvent::DaemonStopping)
        .ok()
        .map(|json| Event::default().data(json))
}

fn build_events_stream(
    rx: tokio::sync::broadcast::Receiver<DaemonEvent>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold(
        (rx, shutdown_rx, true, false),
        |(mut rx, mut shutdown_rx, send_initial_resync, done)| async move {
            if done {
                return None;
            }
            if *shutdown_rx.borrow() {
                return drain_for_daemon_stopping(&mut rx).map(|ev| {
                    (
                        Ok::<Event, Infallible>(ev),
                        (rx, shutdown_rx, false, true),
                    )
                });
            }
            if send_initial_resync
                && let Ok(json) = serde_json::to_string(&DaemonEvent::Resync)
            {
                return Some((
                    Ok::<Event, Infallible>(Event::default().data(json)),
                    (rx, shutdown_rx, false, false),
                ));
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
                                        (rx, shutdown_rx, false, is_stopping),
                                    ));
                                }
                                if is_stopping {
                                    return None;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                if *shutdown_rx.borrow() {
                                    return drain_for_daemon_stopping(&mut rx).map(|ev| {
                                        (
                                            Ok::<Event, Infallible>(ev),
                                            (rx, shutdown_rx, false, true),
                                        )
                                    });
                                }
                                if let Ok(json) = serde_json::to_string(&DaemonEvent::Resync) {
                                    return Some((
                                        Ok::<Event, Infallible>(Event::default().data(json)),
                                        (rx, shutdown_rx, false, false),
                                    ));
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                if *shutdown_rx.borrow() {
                                    return drain_for_daemon_stopping(&mut rx).map(|ev| {
                                        (
                                            Ok::<Event, Infallible>(ev),
                                            (rx, shutdown_rx, false, true),
                                        )
                                    });
                                }
                                return None;
                            }
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            return drain_for_daemon_stopping(&mut rx).map(|ev| {
                                (
                                    Ok::<Event, Infallible>(ev),
                                    (rx, shutdown_rx, false, true),
                                )
                            });
                        }
                        if changed.is_err() {
                            return None;
                        }
                    }
                }
            }
        },
    )
}

async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.supervisor.subscribe();
    let shutdown_rx = state.supervisor.subscribe_shutdown();
    let stream = build_events_stream(rx, shutdown_rx);
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

async fn host_info(State(state): State<AppState>) -> Result<Json<HostInfo>, ApiError> {
    tokio::task::spawn_blocking(move || state.supervisor.host_info())
        .await
        .map(Json)
        .map_err(ApiError::internal)
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

async fn patch_host_config(
    State(state): State<AppState>,
    Json(patch): Json<HostConfigPatch>,
) -> Result<impl IntoResponse, ApiError> {
    let updated = state
        .supervisor
        .patch_host_config(patch)
        .await
        .map_err(ApiError::map_anyhow)?;
    Ok(([(axum::http::header::CONNECTION, "close")], Json(updated)))
}

async fn list_switches(
    State(state): State<AppState>,
) -> Result<Json<Vec<VirtualSwitchInfo>>, ApiError> {
    tokio::task::spawn_blocking(move || state.supervisor.list_switches())
        .await
        .map(Json)
        .map_err(ApiError::internal)
}

async fn create_switch(
    State(state): State<AppState>,
    Json(payload): Json<CreateSwitchPayload>,
) -> Result<(StatusCode, Json<VirtualSwitchInfo>), ApiError> {
    let sup = state.supervisor.clone();
    let created = tokio::task::spawn_blocking(move || sup.create_switch(payload))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::map_anyhow)?;
    state.supervisor.emit(DaemonEvent::SwitchesUpdated);
    Ok((StatusCode::CREATED, Json(created)))
}

async fn delete_switch(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let sup = state.supervisor.clone();
    tokio::task::spawn_blocking(move || sup.delete_switch(&id))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::map_anyhow)?;
    state.supervisor.emit(DaemonEvent::SwitchesUpdated);
    Ok(StatusCode::NO_CONTENT)
}

async fn list_net_adapters(
    State(state): State<AppState>,
) -> Result<Json<Vec<HostNetAdapterInfo>>, ApiError> {
    tokio::task::spawn_blocking(move || state.supervisor.list_net_adapters())
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::map_anyhow)
        .map(Json)
}

#[derive(Deserialize, Default)]
struct DaemonShutdownQuery {
    #[serde(default)]
    stop_vms: bool,
}

async fn daemon_shutdown(
    State(state): State<AppState>,
    Query(query): Query<DaemonShutdownQuery>,
) -> StatusCode {
    state
        .supervisor
        .request_shutdown_with_options(query.stop_vms);
    StatusCode::ACCEPTED
}

async fn create_disk(
    State(state): State<AppState>,
    Json(req): Json<CreateDiskPayload>,
) -> Result<Json<DiskInfo>, ApiError> {
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let _cancel_guard = CancelOnDrop(Arc::clone(&cancel_flag));
    let _shutdown_guard = spawn_cancel_on_shutdown(&state.supervisor, &cancel_flag);
    tokio::task::spawn_blocking(move || state.supervisor.create_disk_cancellable(req, &cancel_flag))
        .await
        .map_err(ApiError::internal)?
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn inspect_disk(
    State(state): State<AppState>,
    Json(req): Json<InspectDiskPayload>,
) -> Result<Json<DiskInfo>, ApiError> {
    tokio::task::spawn_blocking(move || state.supervisor.inspect_disk(&req.path))
        .await
        .map_err(ApiError::internal)?
        .map(Json)
        .map_err(ApiError::map_anyhow)
}

async fn list_vms(State(state): State<AppState>) -> impl IntoResponse {
    let (vms, rev) = state.supervisor.list_vms_with_revision();
    (
        [(
            axum::http::HeaderName::from_static(velovm_core::models::EVENT_REVISION_HEADER),
            rev.to_string(),
        )],
        Json(vms),
    )
}

async fn create_vm(
    State(state): State<AppState>,
    Json(req): Json<CreateVmPayload>,
) -> Result<(StatusCode, Json<VmSummary>), ApiError> {
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let _cancel_guard = CancelOnDrop(Arc::clone(&cancel_flag));
    let _shutdown_guard = spawn_cancel_on_shutdown(&state.supervisor, &cancel_flag);
    let summary =
        tokio::task::spawn_blocking(move || state.supervisor.create_vm_cancellable(req, &cancel_flag))
            .await
            .map_err(ApiError::internal)?
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
        .await
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
        .await
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

#[derive(Debug, Default, Deserialize)]
struct DetachDiskQuery {
    #[serde(default)]
    expected_path: Option<PathBuf>,
}

async fn detach_disk(
    State(state): State<AppState>,
    Path((id, lun)): Path<(String, u32)>,
    Query(query): Query<DetachDiskQuery>,
) -> Result<Json<VmSummary>, ApiError> {
    state
        .supervisor
        .detach_disk_matching(&id, lun, query.expected_path.as_deref())
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
    let content = tokio::task::spawn_blocking(move || state.supervisor.vm_logs(&id, q.tail))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::map_anyhow)?;
    Ok(Json(LogResponse { content }))
}

#[cfg(test)]
mod tests {
    use crate::TestDaemonHandle;

    fn short_tempdir() -> tempfile::TempDir {
        #[cfg(target_os = "macos")]
        {
            tempfile::tempdir_in("/tmp").unwrap()
        }
        #[cfg(not(target_os = "macos"))]
        {
            tempfile::tempdir().unwrap()
        }
    }
    use velovm_core::models::CreateDiskPayload;
    use velovm_core::models::CreateSwitchPayload;
    use velovm_core::models::CreateVmPayload;
    use velovm_core::models::DaemonEvent;
    use velovm_core::models::DiskFormat;
    use velovm_core::models::DiskKind;
    use velovm_core::models::HostNetAdapterInfo;
    use velovm_core::models::IpProto;
    use velovm_core::models::MountIsoPayload;
    use velovm_core::models::NicBackendConfig;
    use velovm_core::models::PortForward;
    use velovm_core::models::PortForwardPayload;
    use velovm_core::models::UpdateVmPayload;
    use velovm_core::models::VmPreset;
    use velovm_core::models::VmStatus;
    use velovm_core::models::VmSummary;
    use std::fs;
    use std::time::Duration;
    use uuid::Uuid;

    fn test_http_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn daemon_rest_api_crud_disk_iso_port_forward_and_auth() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();

        let client = test_http_client();

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

        let client = velovm_core::ttrpc::TtrpcClient::new(&sock);
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
            .properties_vm(velovm_core::vmservice::PropertiesVmRequest { types: vec![] })
            .await
            .unwrap();
        assert_eq!(
            props.state,
            velovm_core::vmservice::VmState::Uninitialized as i32
        );
        assert!(props.fastdesk_listen_addr.is_none());

        let caps = client.capabilities_vm().await.unwrap();
        assert!(!caps.supported_resources.is_empty());

        let _ = client.quit().await;
        let status = child.wait().unwrap();
        assert!(status.success(), "openvmm --rpc exited with {status}");
    }

    #[tokio::test]
    async fn real_openvmm_wait_vm_cancels_on_disconnect_and_reattaches() {
        use velovm_core::ttrpc::{TtrpcClient, WaitVmOutcome};
        use velovm_core::vmservice;

        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir.join("../../..");
        let bin_name = if cfg!(windows) { "openvmm.exe" } else { "openvmm" };
        let openvmm_bin = [
            repo_root.join("openvmm/target/release").join(bin_name),
            repo_root.join("openvmm/target/debug").join(bin_name),
        ]
        .into_iter()
        .find(|p| p.is_file());
        let msvm_fd = repo_root.join(if cfg!(target_arch = "aarch64") {
            "openvmm/.packages/hyperv.uefi.mscoreuefi.AARCH64.RELEASE/MsvmAARCH64/RELEASE_CLANGPDB/FV/MSVM.fd"
        } else {
            "openvmm/.packages/hyperv.uefi.mscoreuefi.x64.RELEASE/MsvmX64/RELEASE_VS2022/FV/MSVM.fd"
        });
        #[cfg(target_os = "linux")]
        let (hypervisor_ready, hypervisor_reason) = (
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .is_ok(),
            "/dev/kvm is not accessible read/write",
        );
        #[cfg(windows)]
        let (hypervisor_ready, hypervisor_reason) = (true, "");
        #[cfg(target_os = "macos")]
        let (hypervisor_ready, hypervisor_reason) = (
            cfg!(target_arch = "aarch64"),
            "macOS HVF requires Apple Silicon (aarch64)",
        );
        #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
        let (hypervisor_ready, hypervisor_reason) =
            (false, "no supported hypervisor on this platform");

        let missing_reason = if openvmm_bin.is_none() {
            Some(format!(
                "real openvmm binary not found under {}",
                repo_root.display()
            ))
        } else if !msvm_fd.is_file() {
            Some(format!("UEFI firmware not found at {}", msvm_fd.display()))
        } else if !hypervisor_ready {
            Some(hypervisor_reason.into())
        } else {
            None
        };
        if let Some(reason) = missing_reason {
            if std::env::var_os("OPENVMM_REQUIRE_REAL_KVM_TEST").is_some() {
                panic!(
                    "OPENVMM_REQUIRE_REAL_KVM_TEST is set, cannot run real WaitVM test: {reason}"
                );
            }
            eprintln!("skipping real_openvmm_wait_vm_cancels_on_disconnect_and_reattaches: {reason}");
            return;
        }
        let openvmm_bin = openvmm_bin.unwrap();

        struct ChildKillGuard(Option<std::process::Child>);
        impl Drop for ChildKillGuard {
            fn drop(&mut self) {
                if let Some(mut c) = self.0.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("openvmm-wait.sock");
        let disk_path = dir.path().join("boot.img");
        fs::write(&disk_path, vec![0u8; 1024 * 1024]).unwrap();
        let vmgs_path = dir.path().join("state.vmgs");

        let child = std::process::Command::new(&openvmm_bin)
            .arg("--rpc")
            .arg(format!("path={},transport=ttrpc", sock.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut child_guard = ChildKillGuard(Some(child));

        let client = TtrpcClient::new(&sock);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if sock.exists() && client.ping().await.is_ok() {
                break;
            }
            if let Some(status) = child_guard.0.as_mut().unwrap().try_wait().unwrap() {
                panic!("openvmm --rpc exited prematurely with {status}");
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for openvmm --rpc socket"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let mut cfg = velovm_core::models::VmConfig::from_preset(
            "RealWaitVm",
            VmPreset::Custom,
            dir.path(),
        );
        cfg.vcpus = 1;
        cfg.memory_mb = 512;
        cfg.tpm_enabled = false;
        cfg.fastdesk.enabled = false;
        cfg.nics.clear();
        cfg.vmgs_path = Some(vmgs_path);
        cfg.disks = vec![velovm_core::models::DiskAttachment {
            lun: 0,
            kind: DiskKind::HardDisk,
            path: disk_path,
            read_only: false,
            format: DiskFormat::Raw,
        }];
        let create_req =
            crate::supervisor::build_create_vm_request(&cfg, &msvm_fd, None).unwrap();
        if let Err(err) = client.create_vm(create_req).await {
            let msg = format!("{err:#}");
            let lower = msg.to_ascii_lowercase();
            let hypervisor_missing = (cfg!(windows) || cfg!(target_os = "macos"))
                && (lower.contains("hypervisor")
                    || lower.contains("whp")
                    || lower.contains("hvf")
                    || lower.contains("hv_denied")
                    || lower.contains("hv_unsupported")
                    || lower.contains("hv_error")
                    || lower.contains("entitled")
                    || lower.contains("unsupported")
                    || lower.contains("partition")
                    || lower.contains("virtualization")
                    || lower.contains("not present")
                    || lower.contains("not enabled"));
            if hypervisor_missing && std::env::var_os("OPENVMM_REQUIRE_REAL_KVM_TEST").is_none() {
                let hv_name = if cfg!(target_os = "macos") { "HVF" } else { "WHP" };
                eprintln!(
                    "skipping rest of real_openvmm_wait_vm_cancels_on_disconnect_and_reattaches after spawning openvmm --rpc ({hv_name} unavailable): {msg}"
                );
                let _ = client.quit().await;
                let status = child_guard.0.take().unwrap().wait().unwrap();
                assert!(status.success(), "openvmm --rpc exited with {status}");
                return;
            }
            panic!("CreateVM RPC failed: {msg}");
        }
        client.resume_vm().await.unwrap();

        let props = client
            .properties_vm(vmservice::PropertiesVmRequest { types: vec![] })
            .await
            .unwrap();
        assert_eq!(props.state, vmservice::VmState::Running as i32);

        // 1. Task 1 holds the single WaitVM slot on the real openvmm process.
        let (cancel_tx1, cancel_rx1) = tokio::sync::watch::channel(false);
        let task1 = tokio::spawn({
            let c = client.clone();
            async move { c.wait_vm_cancellable(cancel_rx1).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert!(!task1.is_finished(), "Task 1 exited before probe1");

        let (probe1_cancel_tx, probe1_cancel_rx) = tokio::sync::watch::channel(false);
        let probe1_err = tokio::select! {
            r = client.wait_vm_cancellable(probe1_cancel_rx) => r.unwrap_err(),
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                let _ = probe1_cancel_tx.send(true);
                panic!("probe1 did not fail with 'wait VM already in flight'");
            }
        };
        assert!(
            format!("{probe1_err:#}").contains("wait VM already in flight"),
            "unexpected probe1 error: {probe1_err:#}"
        );

        // 2. Cancelling Task 1 shuts down its socket, cancelling WaitVM in real openvmm.
        let _ = cancel_tx1.send(true);
        let outcome1 = tokio::time::timeout(std::time::Duration::from_secs(2), task1)
            .await
            .expect("Task 1 did not unblock on cancel")
            .unwrap()
            .unwrap();
        assert_eq!(outcome1, WaitVmOutcome::Cancelled);

        // 3. Re-attaching Task 2 succeeds directly without hitting "wait VM already in flight".
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let (_cancel_tx2, cancel_rx2) = tokio::sync::watch::channel(false);
        let task2 = tokio::spawn({
            let c = client.clone();
            async move { c.wait_vm_cancellable(cancel_rx2).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !task2.is_finished(),
            "Task 2 exited prematurely instead of holding WaitVM"
        );

        let (probe2_cancel_tx, probe2_cancel_rx) = tokio::sync::watch::channel(false);
        let probe2_err = tokio::select! {
            r = client.wait_vm_cancellable(probe2_cancel_rx) => r.unwrap_err(),
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                let _ = probe2_cancel_tx.send(true);
                panic!("probe2 did not fail with 'wait VM already in flight'");
            }
        };
        assert!(
            format!("{probe2_err:#}").contains("wait VM already in flight"),
            "unexpected probe2 error: {probe2_err:#}"
        );

        // 4. Tearing down the VM resolves Task 2's WaitVM cleanly.
        client.teardown_vm().await.unwrap();
        let wait2_err = tokio::time::timeout(std::time::Duration::from_secs(5), task2)
            .await
            .expect("Task 2 did not finish after teardown_vm")
            .unwrap()
            .unwrap_err();
        let wait2_msg = format!("{wait2_err:#}");
        assert!(
            wait2_msg.contains("VM torn down"),
            "expected 'VM torn down' from Task 2, got: {wait2_msg}"
        );
        assert!(
            !wait2_msg.contains("wait VM already in flight"),
            "Task 2 failed with stale WaitVM conflict: {wait2_msg}"
        );

        let _ = client.quit().await;
        let status = child_guard.0.take().unwrap().wait().unwrap();
        assert!(status.success(), "openvmm --rpc exited with {status}");
    }

    #[tokio::test]
    async fn delete_vm_without_delete_disks_preserves_default_and_vm_dir_disks() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

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
            .json(&velovm_core::models::AttachDiskPayload {
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
        let client = test_http_client();

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
                disks: vec![velovm_core::models::DiskAttachment {
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
        let client = test_http_client();

        // Open a persistent SSE stream using bearer authentication.
        let mut sse_resp = client
            .get(format!("{}/api/v1/events", daemon.base_url))
            .bearer_auth(&daemon.token)
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
    async fn sse_events_stream_delivers_daemon_stopping_when_events_queued_or_lagged_at_shutdown() {
        use super::build_events_stream;
        use futures_util::StreamExt;
        use tokio::sync::{broadcast, watch};
        use uuid::Uuid;
        use velovm_core::models::DaemonEvent;

        // Case 1: An ordinary event is queued before DaemonStopping and shutdown watch flipped before polling.
        {
            let (tx, rx) = broadcast::channel::<DaemonEvent>(16);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let stream = build_events_stream(rx, shutdown_rx);
            tokio::pin!(stream);

            // Consume initial Resync so stream enters the select loop on next poll.
            let first = stream.next().await.unwrap().unwrap();
            let first_debug = format!("{first:?}");
            assert!(first_debug.contains("resync"), "unexpected first event: {first_debug}");

            // Queue VmDeleted + HostConfigUpdated followed by DaemonStopping + shutdown watch before polling again.
            tx.send(DaemonEvent::VmDeleted {
                id: Uuid::nil(),
                revision: 1,
            })
            .unwrap();
            tx.send(DaemonEvent::HostConfigUpdated).unwrap();
            tx.send(DaemonEvent::DaemonStopping).unwrap();
            shutdown_tx.send(true).unwrap();

            let mut saw_stopping = false;
            while let Some(item) = stream.next().await {
                let ev = item.unwrap();
                if format!("{ev:?}").contains("daemon_stopping") {
                    saw_stopping = true;
                }
            }
            assert!(
                saw_stopping,
                "expected daemon_stopping before EOF when other events were queued before shutdown"
            );
        }

        // Case 2: Subscriber lagged past channel capacity before shutdown watch fires.
        {
            let (tx, rx) = broadcast::channel::<DaemonEvent>(4);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let stream = build_events_stream(rx, shutdown_rx);
            tokio::pin!(stream);

            let _ = stream.next().await.unwrap().unwrap();

            for revision in 1..=8 {
                let _ = tx.send(DaemonEvent::VmDeleted {
                    id: Uuid::nil(),
                    revision,
                });
            }
            let _ = tx.send(DaemonEvent::DaemonStopping);
            shutdown_tx.send(true).unwrap();

            let mut saw_stopping = false;
            while let Some(item) = stream.next().await {
                let ev = item.unwrap();
                if format!("{ev:?}").contains("daemon_stopping") {
                    saw_stopping = true;
                }
            }
            assert!(
                saw_stopping,
                "expected daemon_stopping before EOF even when subscriber lagged"
            );
        }

        // Case 3: Shutdown starts before the stream is even polled once.
        {
            let (tx, rx) = broadcast::channel::<DaemonEvent>(4);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let _ = tx.send(DaemonEvent::DaemonStopping);
            shutdown_tx.send(true).unwrap();

            let stream = build_events_stream(rx, shutdown_rx);
            tokio::pin!(stream);
            let mut saw_stopping = false;
            while let Some(item) = stream.next().await {
                let ev = item.unwrap();
                if format!("{ev:?}").contains("daemon_stopping") {
                    saw_stopping = true;
                }
            }
            assert!(
                saw_stopping,
                "expected daemon_stopping before EOF when shutdown precedes first poll"
            );
        }
    }

    #[tokio::test]
    async fn updating_host_config_listen_addr_serves_new_port_on_same_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

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

        let mut cfg: velovm_core::models::HostConfig = client
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
        let updated: velovm_core::models::HostConfig = put_resp.json().await.unwrap();
        assert_eq!(updated.listen_addr, "127.0.0.1:0");

        // ApiClient::from_data_dir resolves the persisted new listen_addr and talks to the SAME daemon.
        let data_dir = dir.path().to_path_buf();
        let old_base_url = daemon.base_url.clone();
        let (health_ok, vm_names): (bool, Vec<String>) = tokio::task::spawn_blocking(move || {
            let api = velovm_core::api_client::ApiClient::from_data_dir(Some(&data_dir), None)
                .unwrap();
            assert_ne!(api.base_url(), old_base_url);
            api.health().unwrap();
            assert_eq!(
                format!("http://{}", api.host_info().unwrap().listen_addr),
                api.base_url()
            );
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
        let client = test_http_client();

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
            .set_vm_status_for_test(created.config.id, velovm_core::models::VmStatus::Starting);

        let disk_path = dir.path().join("extra.img");
        fs::write(&disk_path, b"DISK").unwrap();
        let attach_resp = client
            .post(format!("{}/api/v1/vms/Starting-VM/disks", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&velovm_core::models::AttachDiskPayload {
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
            .set_vm_status_for_test(created.config.id, velovm_core::models::VmStatus::Off);
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn delayed_pause_resume_result_does_not_clobber_stop_or_restart() {
        use velovm_core::models::VmStatus;
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
        let client = test_http_client();

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

    #[tokio::test]
    async fn failed_create_vm_does_not_leave_created_disk_behind_and_retry_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

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

        // Exercise post-disk-creation rollback: `create_disk_cancellable` succeeds
        // and creates `disk_path`, then `save_vm` fails because `vms/` is blocked
        // by a regular file; the newly created disk must be removed cleanly.
        let vms_dir = dir.path().join("vms");
        fs::remove_dir(&vms_dir).unwrap();
        fs::write(&vms_dir, b"blocker").unwrap();
        let rollback_resp = client
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
        assert!(
            !rollback_resp.status().is_success(),
            "expected failure when save_vm cannot create vms/<id>"
        );
        assert!(
            !disk_path.exists(),
            "post-disk-creation rollback failed to remove {}",
            disk_path.display()
        );
        fs::remove_file(&vms_dir).unwrap();
        fs::create_dir_all(&vms_dir).unwrap();

        // Retrying with corrected state using the same disk path must now succeed.
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

    #[tokio::test]
    async fn standalone_create_disk_rejects_incompatible_extension() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

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

    #[tokio::test]
    async fn delete_disks_preserves_disk_referenced_by_another_vm() {
        let dir = tempfile::tempdir().unwrap();
        let shared_disk = dir.path().join("shared.img");
        let private_disk = dir.path().join("private.img");
        fs::write(&shared_disk, b"SHARED").unwrap();
        fs::write(&private_disk, b"PRIVATE").unwrap();

        let mk_disk = |lun: u32, path: std::path::PathBuf| velovm_core::models::DiskAttachment {
            lun,
            kind: DiskKind::HardDisk,
            path,
            read_only: false,
            format: DiskFormat::Raw,
        };

        // Pre-seed two persisted VM configs referencing the same image to verify
        // `delete_vm(delete_disks=true)` preserves disks still referenced elsewhere.
        let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
        let mut owner1 = velovm_core::models::VmConfig::from_preset(
            "VM-Owner-1",
            VmPreset::Custom,
            &store.vm_dir(uuid::Uuid::new_v4()),
        );
        owner1.disks = vec![
            mk_disk(0, shared_disk.clone()),
            mk_disk(1, private_disk.clone()),
        ];
        store.save_vm(&owner1).unwrap();

        let mut owner2 = velovm_core::models::VmConfig::from_preset(
            "VM-Owner-2",
            VmPreset::Custom,
            &store.vm_dir(uuid::Uuid::new_v4()),
        );
        owner2.disks = vec![mk_disk(0, shared_disk.clone())];
        store.save_vm(&owner2).unwrap();

        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let del = client
            .delete(format!(
                "{}/api/v1/vms/VM-Owner-1?delete_disks=true",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(del.status(), 204);
        assert!(shared_disk.exists(), "shared disk must not be deleted");
        assert!(!private_disk.exists(), "unshared private disk should be deleted");

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn host_config_rejects_non_loopback_listen_addr_and_runtime_dir_change_while_active() {
        let dir = short_tempdir();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let base_cfg: velovm_core::models::HostConfig = client
            .get(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let mut non_loopback = base_cfg.clone();
        non_loopback.listen_addr = "0.0.0.0:47620".into();
        assert_eq!(
            client
                .put(format!("{}/api/v1/host/config", daemon.base_url))
                .bearer_auth(&daemon.token)
                .json(&non_loopback)
                .send()
                .await
                .unwrap()
                .status(),
            400
        );

        let mut changed_data = base_cfg.clone();
        changed_data.data_dir = dir.path().join("other-data");
        assert_eq!(
            client
                .put(format!("{}/api/v1/host/config", daemon.base_url))
                .bearer_auth(&daemon.token)
                .json(&changed_data)
                .send()
                .await
                .unwrap()
                .status(),
            400
        );

        let created: VmSummary = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "Active-RuntimeDir-VM".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                ..Default::default()
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        daemon
            .supervisor
            .set_vm_status_for_test(created.config.id, VmStatus::Running);
        let mut changed_runtime = base_cfg.clone();
        changed_runtime.runtime_dir = dir.path().join("alt-runtime");
        assert_eq!(
            client
                .put(format!("{}/api/v1/host/config", daemon.base_url))
                .bearer_auth(&daemon.token)
                .json(&changed_runtime)
                .send()
                .await
                .unwrap()
                .status(),
            409
        );

        daemon
            .supervisor
            .set_vm_status_for_test(created.config.id, VmStatus::Off);
        assert_eq!(
            client
                .put(format!("{}/api/v1/host/config", daemon.base_url))
                .bearer_auth(&daemon.token)
                .json(&changed_runtime)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn listen_override_does_not_overwrite_config_toml() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();

        let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
        let persisted = store.load_or_init_host_config().unwrap();
        assert_eq!(
            persisted.listen_addr,
            velovm_core::models::DEFAULT_DAEMON_LISTEN
        );
        let runtime_addr = store.read_runtime_listen_addr().unwrap();
        assert_eq!(format!("http://{runtime_addr}"), daemon.base_url);

        let data_dir = dir.path().to_path_buf();
        let expected_url = daemon.base_url.clone();
        tokio::task::spawn_blocking(move || {
            let api = velovm_core::api_client::ApiClient::from_data_dir(Some(&data_dir), None)
                .unwrap();
            assert_eq!(api.base_url(), expected_url);
            api.health().unwrap();
        })
        .await
        .unwrap();

        daemon.shutdown().await;
        assert!(store.read_runtime_listen_addr().is_none());
        assert_eq!(
            store.load_or_init_host_config().unwrap().listen_addr,
            velovm_core::models::DEFAULT_DAEMON_LISTEN
        );
    }

    #[test]
    fn api_error_status_codes_use_typed_errors_not_substrings() {
        use super::ApiError;
        use axum::http::StatusCode;
        // Wrap each typed error with misleading substrings to ensure text never drives status codes.
        let not_found = ApiError::map_anyhow(
            crate::supervisor::err_not_found("missing item")
                .context("cannot proceed: already exists"),
        );
        assert_eq!(not_found.status, StatusCode::NOT_FOUND);

        let conflict = ApiError::map_anyhow(
            crate::supervisor::err_conflict("duplicate state").context("not found wrapper"),
        );
        assert_eq!(conflict.status, StatusCode::CONFLICT);

        let bad_req = ApiError::map_anyhow(
            crate::supervisor::err_bad_request("invalid size").context("cannot allocate; not found"),
        );
        assert_eq!(bad_req.status, StatusCode::BAD_REQUEST);

        let internal = ApiError::map_anyhow(anyhow::anyhow!("not found / cannot / already exists"));
        assert_eq!(internal.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn create_vm_with_iso_in_disks_list_normalizes_to_readonly_dvd() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let iso_path = dir.path().join("installer.iso");
        fs::write(&iso_path, b"ISO").unwrap();

        let resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "Iso-Normalize-VM".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                disks: vec![velovm_core::models::DiskAttachment {
                    lun: 0,
                    kind: DiskKind::HardDisk,
                    path: iso_path,
                    read_only: false,
                    format: DiskFormat::Raw,
                }],
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let vm: VmSummary = resp.json().await.unwrap();
        assert_eq!(vm.config.disks[0].kind, DiskKind::Dvd);
        assert!(vm.config.disks[0].read_only);
        assert_eq!(vm.config.disks[0].format, DiskFormat::Iso);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn auth_middleware_rejects_missing_wrong_empty_truncated_and_query_tokens_across_all_protected_routes() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();
        let truncated = &daemon.token[..daemon.token.len().saturating_sub(1)];

        let routes: Vec<(reqwest::Method, &str)> = vec![
            (reqwest::Method::GET, "/api/v1/events"),
            (reqwest::Method::GET, "/api/v1/host/info"),
            (reqwest::Method::GET, "/api/v1/host/config"),
            (reqwest::Method::PUT, "/api/v1/host/config"),
            (reqwest::Method::GET, "/api/v1/host/switches"),
            (reqwest::Method::POST, "/api/v1/host/switches"),
            (reqwest::Method::DELETE, "/api/v1/host/switches/some-switch"),
            (reqwest::Method::GET, "/api/v1/host/net-adapters"),
            (reqwest::Method::POST, "/api/v1/daemon/shutdown"),
            (reqwest::Method::POST, "/api/v1/disks/create"),
            (reqwest::Method::POST, "/api/v1/disks/inspect"),
            (reqwest::Method::GET, "/api/v1/vms"),
            (reqwest::Method::POST, "/api/v1/vms"),
            (
                reqwest::Method::GET,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000",
            ),
            (
                reqwest::Method::PUT,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000",
            ),
            (
                reqwest::Method::DELETE,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000",
            ),
            (
                reqwest::Method::POST,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/start",
            ),
            (
                reqwest::Method::POST,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/stop",
            ),
            (
                reqwest::Method::POST,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/shutdown",
            ),
            (
                reqwest::Method::POST,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/pause",
            ),
            (
                reqwest::Method::POST,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/resume",
            ),
            (
                reqwest::Method::POST,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/reset",
            ),
            (
                reqwest::Method::POST,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/disks",
            ),
            (
                reqwest::Method::DELETE,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/disks/0",
            ),
            (
                reqwest::Method::POST,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/iso",
            ),
            (
                reqwest::Method::DELETE,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/iso",
            ),
            (
                reqwest::Method::DELETE,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/iso/0",
            ),
            (
                reqwest::Method::POST,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/nics/00000000-0000-0000-0000-000000000000/ports",
            ),
            (
                reqwest::Method::DELETE,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/nics/00000000-0000-0000-0000-000000000000/ports",
            ),
            (
                reqwest::Method::GET,
                "/api/v1/vms/00000000-0000-0000-0000-000000000000/logs",
            ),
        ];

        for (method, path) in &routes {
            for case in ["missing", "wrong", "empty", "truncated", "query"] {
                let req = match case {
                    "missing" => client.request(method.clone(), format!("{}{path}", daemon.base_url)),
                    "wrong" => client
                        .request(method.clone(), format!("{}{path}", daemon.base_url))
                        .bearer_auth("definitely-not-the-valid-token"),
                    "empty" => client
                        .request(method.clone(), format!("{}{path}", daemon.base_url))
                        .header(reqwest::header::AUTHORIZATION, "Bearer "),
                    "truncated" => client
                        .request(method.clone(), format!("{}{path}", daemon.base_url))
                        .bearer_auth(truncated),
                    "query" => client.request(
                        method.clone(),
                        format!("{}{path}?token={}", daemon.base_url, daemon.token),
                    ),
                    _ => unreachable!(),
                };
                let resp = req.send().await.unwrap();
                assert_eq!(
                    resp.status(),
                    401,
                    "route {method} {path} ({case}) must return 401"
                );
            }
        }

        let health = client
            .get(format!("{}/api/v1/health", daemon.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), 200);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn attach_and_create_vm_reject_duplicate_writable_disk_and_vhdx_magic_as_raw() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let disk_a = dir.path().join("disk-a.img");
        let disk_b = dir.path().join("disk-b.img");
        let shared_ro = dir.path().join("shared-ro.img");
        let disguised_vhdx = dir.path().join("checkpoint.dat");
        fs::write(&disk_a, vec![0u8; 4096]).unwrap();
        fs::write(&disk_b, vec![0u8; 4096]).unwrap();
        fs::write(&shared_ro, vec![0u8; 4096]).unwrap();
        fs::write(&disguised_vhdx, b"vhdxfile\0\0\0\0\0\0\0\0").unwrap();

        // 1. Creating VM-1 with disguised VHDX marked as raw fails cleanly.
        let bad_create = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "BadMagic-VM".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                disks: vec![velovm_core::models::DiskAttachment {
                    lun: 0,
                    kind: DiskKind::HardDisk,
                    path: disguised_vhdx.clone(),
                    read_only: false,
                    format: DiskFormat::Raw,
                }],
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(bad_create.status(), 400);

        // 2. Create VM-1 owning writable disk_a + read-only shared_ro.
        let vm1_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "DiskGuard-1".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                disks: vec![
                    velovm_core::models::DiskAttachment {
                        lun: 0,
                        kind: DiskKind::HardDisk,
                        path: disk_a.clone(),
                        read_only: false,
                        format: DiskFormat::Raw,
                    },
                    velovm_core::models::DiskAttachment {
                        lun: 2,
                        kind: DiskKind::HardDisk,
                        path: shared_ro.clone(),
                        read_only: true,
                        format: DiskFormat::Raw,
                    },
                ],
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(vm1_resp.status(), 201);

        // 3. Re-attaching disk_a on another LUN of VM-1 fails with 409 Conflict.
        let dup_same_vm = client
            .post(format!("{}/api/v1/vms/DiskGuard-1/disks", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&velovm_core::models::AttachDiskPayload {
                path: disk_a.clone(),
                lun: Some(3),
                read_only: false,
                format: Some(DiskFormat::Raw),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(dup_same_vm.status(), 409);

        // 4. Creating VM-2 with writable disk_a fails with 409 Conflict.
        let conflict_create = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "DiskGuard-2".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                disks: vec![velovm_core::models::DiskAttachment {
                    lun: 0,
                    kind: DiskKind::HardDisk,
                    path: disk_a.clone(),
                    read_only: false,
                    format: DiskFormat::Raw,
                }],
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(conflict_create.status(), 409);

        // 5. Create VM-2 with disk_b + read-only shared_ro -> 201 (both RO allowed).
        let vm2_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "DiskGuard-2".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                disks: vec![
                    velovm_core::models::DiskAttachment {
                        lun: 0,
                        kind: DiskKind::HardDisk,
                        path: disk_b.clone(),
                        read_only: false,
                        format: DiskFormat::Raw,
                    },
                    velovm_core::models::DiskAttachment {
                        lun: 1,
                        kind: DiskKind::HardDisk,
                        path: shared_ro,
                        read_only: true,
                        format: DiskFormat::Raw,
                    },
                ],
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(vm2_resp.status(), 201);

        // 6. Attaching disk_a (writable or read-only) to VM-2 fails with 409 Conflict.
        let cross_attach = client
            .post(format!("{}/api/v1/vms/DiskGuard-2/disks", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&velovm_core::models::AttachDiskPayload {
                path: disk_a,
                lun: Some(2),
                read_only: true,
                format: Some(DiskFormat::Raw),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(cross_attach.status(), 409);

        // 7. Attaching disguised_vhdx with inferred format (format: None) fails with 400.
        let inferred_vhdx = client
            .post(format!("{}/api/v1/vms/DiskGuard-2/disks", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&velovm_core::models::AttachDiskPayload {
                path: disguised_vhdx,
                lun: Some(3),
                read_only: false,
                format: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(inferred_vhdx.status(), 400);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn sse_events_stream_emits_initial_resync() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let mut sse_resp = client
            .get(format!("{}/api/v1/events", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(sse_resp.status(), 200);

        let first_chunk = tokio::time::timeout(std::time::Duration::from_secs(2), sse_resp.chunk())
            .await
            .expect("timed out waiting for initial SSE resync event")
            .unwrap()
            .expect("expected initial SSE chunk before EOF");
        let text = String::from_utf8_lossy(&first_chunk);
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("expected SSE data line in first chunk");
        let parsed: velovm_core::models::DaemonEvent =
            serde_json::from_str(data).expect("valid DaemonEvent JSON");
        assert!(
            matches!(parsed, velovm_core::models::DaemonEvent::Resync),
            "expected initial Resync SSE event, got: {parsed:?}"
        );

        daemon.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_vm_preserves_existing_custom_runtime_dir_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = short_tempdir();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let custom_runtime = dir.path().join("custom-runtime");
        fs::create_dir_all(&custom_runtime).unwrap();
        fs::set_permissions(&custom_runtime, fs::Permissions::from_mode(0o755)).unwrap();

        let fake_openvmm = dir.path().join("fake-openvmm.sh");
        fs::write(&fake_openvmm, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&fake_openvmm, fs::Permissions::from_mode(0o755)).unwrap();

        let fake_uefi = dir.path().join("MSVM.fd");
        fs::write(&fake_uefi, b"uefi").unwrap();

        let mut host_cfg: velovm_core::models::HostConfig = client
            .get(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        host_cfg.runtime_dir = custom_runtime.clone();
        host_cfg.openvmm_bin = Some(fake_openvmm);
        host_cfg.uefi_firmware_path = Some(fake_uefi);

        let put_resp = client
            .put(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&host_cfg)
            .send()
            .await
            .unwrap();
        assert_eq!(put_resp.status(), 200);

        let vm_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "PermTestVm".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(vm_resp.status(), 201);

        let _start_resp = client
            .post(format!("{}/api/v1/vms/PermTestVm/start", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();

        let mode = fs::metadata(&custom_runtime).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "existing custom runtime_dir permissions should be preserved (got {mode:o})"
        );

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn failed_host_config_save_keeps_old_listen_port() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let initial_cfg: velovm_core::models::HostConfig = client
            .get(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        // Find a free ephemeral port that update_host_config would try to bind.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let new_addr = probe.local_addr().unwrap().to_string();
        drop(probe);

        // Replace config.toml with a non-empty directory so atomic_write/rename fails.
        let cfg_path = dir.path().join("config.toml");
        if cfg_path.exists() {
            fs::remove_file(&cfg_path).unwrap();
        }
        fs::create_dir_all(&cfg_path).unwrap();
        fs::write(cfg_path.join("blocker"), b"x").unwrap();

        let mut next_cfg = initial_cfg.clone();
        next_cfg.listen_addr = new_addr.clone();
        let put_resp = client
            .put(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&next_cfg)
            .send()
            .await
            .unwrap();
        assert!(
            put_resp.status().is_server_error(),
            "expected config save failure to return 5xx, got {}",
            put_resp.status()
        );

        // Old endpoint still serves and still holds the original listen_addr.
        let current_cfg: velovm_core::models::HostConfig = client
            .get(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(current_cfg.listen_addr, initial_cfg.listen_addr);

        let recorded_addr = fs::read_to_string(dir.path().join("daemon.addr")).unwrap();
        assert_eq!(format!("http://{}", recorded_addr.trim()), daemon.base_url);
        // The tentatively bound new listener must be released after config save rollback.
        let _rebound = std::net::TcpListener::bind(&new_addr)
            .expect("new listen address should be released after failed config save");

        // Clean up blocker dir before shutdown.
        fs::remove_dir_all(&cfg_path).unwrap();
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn patch_host_config_in_use_listen_addr_returns_409_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied_addr = occupied.local_addr().unwrap().to_string();

        let resp = client
            .patch(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&serde_json::json!({ "listen_addr": occupied_addr }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::CONFLICT,
            "occupied listen_addr must return 409 Conflict instead of 500"
        );

        drop(occupied);
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn relative_disk_paths_are_rejected_across_disk_and_vm_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let create_disk_resp = client
            .post(format!("{}/api/v1/disks/create", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateDiskPayload {
                path: std::path::PathBuf::from("relative/os.img"),
                size_bytes: 1024 * 1024 * 1024,
                format: DiskFormat::Raw,
                fixed: false,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(create_disk_resp.status(), 400);

        let inspect_resp = client
            .post(format!("{}/api/v1/disks/inspect", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&velovm_core::models::InspectDiskPayload {
                path: std::path::PathBuf::from("relative/os.img"),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(inspect_resp.status(), 400);

        let create_vm_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "RelDiskVm".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                disks: vec![velovm_core::models::DiskAttachment {
                    lun: 0,
                    kind: DiskKind::HardDisk,
                    path: std::path::PathBuf::from("relative/os.img"),
                    read_only: false,
                    format: DiskFormat::Raw,
                }],
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(create_vm_resp.status(), 400);

        let valid_vm: VmSummary = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "RelAttachVm".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                ..Default::default()
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let attach_resp = client
            .post(format!(
                "{}/api/v1/vms/{}/disks",
                daemon.base_url, valid_vm.config.id
            ))
            .bearer_auth(&daemon.token)
            .json(&velovm_core::models::AttachDiskPayload {
                path: std::path::PathBuf::from("relative/extra.img"),
                lun: Some(1),
                read_only: false,
                format: Some(DiskFormat::Raw),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(attach_resp.status(), 400);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn overlapping_port_forwards_are_rejected_on_single_and_multiple_nics() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let nic1_id = uuid::Uuid::new_v4();
        let nic2_id = uuid::Uuid::new_v4();
        let vm: VmSummary = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "PortOverlapVm".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                nics: Some(vec![
                    velovm_core::models::NicAttachment {
                        id: nic1_id,
                        name: "nat0".into(),
                        mac_address: "00-15-5D-11-22-33".into(),
                        backend: NicBackendConfig::Consomme {
                            cidr: None,
                            port_forwards: vec![PortForward {
                                host_port: 8080,
                                guest_port: 80,
                                protocol: IpProto::Tcp,
                                host_addr: "127.0.0.1".into(),
                            }],
                            internal_dns: true,
                            allow_host_local_access: true,
                        },
                    },
                    velovm_core::models::NicAttachment {
                        id: nic2_id,
                        name: "nat1".into(),
                        mac_address: "00-15-5D-11-22-34".into(),
                        backend: NicBackendConfig::Consomme {
                            cidr: None,
                            port_forwards: vec![],
                            internal_dns: true,
                            allow_host_local_access: true,
                        },
                    },
                ]),
                ..Default::default()
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        // Overlapping rule on same NIC with different guest port -> 409 Conflict.
        let same_nic_overlap = client
            .post(format!(
                "{}/api/v1/vms/{}/nics/{nic1_id}/ports",
                daemon.base_url, vm.config.id
            ))
            .bearer_auth(&daemon.token)
            .json(&PortForwardPayload {
                port: PortForward {
                    host_port: 8080,
                    guest_port: 8081,
                    protocol: IpProto::Tcp,
                    host_addr: "127.0.0.1".into(),
                },
            })
            .send()
            .await
            .unwrap();
        assert_eq!(same_nic_overlap.status(), 409);

        // Wildcard bind on second NIC overlaps 127.0.0.1:8080 on NIC 1 -> 409 Conflict.
        let cross_nic_wildcard = client
            .post(format!(
                "{}/api/v1/vms/{}/nics/{nic2_id}/ports",
                daemon.base_url, vm.config.id
            ))
            .bearer_auth(&daemon.token)
            .json(&PortForwardPayload {
                port: PortForward {
                    host_port: 8080,
                    guest_port: 8443,
                    protocol: IpProto::Tcp,
                    host_addr: "0.0.0.0".into(),
                },
            })
            .send()
            .await
            .unwrap();
        assert_eq!(cross_nic_wildcard.status(), 409);

        // IPv6 wildcard on same host port and guest port coexists with IPv4 rule (IPV6_V6ONLY) -> 200 OK.
        let ipv6_coexist_ok = client
            .post(format!(
                "{}/api/v1/vms/{}/nics/{nic1_id}/ports",
                daemon.base_url, vm.config.id
            ))
            .bearer_auth(&daemon.token)
            .json(&PortForwardPayload {
                port: PortForward {
                    host_port: 8080,
                    guest_port: 80,
                    protocol: IpProto::Tcp,
                    host_addr: "::".into(),
                },
            })
            .send()
            .await
            .unwrap();
        assert_eq!(ipv6_coexist_ok.status(), 200);

        // Same NIC duplicate (protocol, IP family, guest_port) with a different host port -> 409 Conflict.
        let same_nic_guest_dup = client
            .post(format!(
                "{}/api/v1/vms/{}/nics/{nic1_id}/ports",
                daemon.base_url, vm.config.id
            ))
            .bearer_auth(&daemon.token)
            .json(&PortForwardPayload {
                port: PortForward {
                    host_port: 8081,
                    guest_port: 80,
                    protocol: IpProto::Tcp,
                    host_addr: "127.0.0.1".into(),
                },
            })
            .send()
            .await
            .unwrap();
        assert_eq!(same_nic_guest_dup.status(), 409);

        // Different protocol (UDP) on the same host port is allowed -> 200 OK.
        let udp_ok = client
            .post(format!(
                "{}/api/v1/vms/{}/nics/{nic2_id}/ports",
                daemon.base_url, vm.config.id
            ))
            .bearer_auth(&daemon.token)
            .json(&PortForwardPayload {
                port: PortForward {
                    host_port: 8080,
                    guest_port: 5353,
                    protocol: IpProto::Udp,
                    host_addr: String::new(),
                },
            })
            .send()
            .await
            .unwrap();
        assert_eq!(udp_ok.status(), 200);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn dropping_update_and_delete_futures_midflight_still_completes_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let vm: VmSummary = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "DisconnectSafeVm".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                ..Default::default()
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        // Start update_vm and drop its caller future after one poll.
        {
            let update_fut = daemon.supervisor.update_vm(
                &vm.config.name,
                UpdateVmPayload {
                    description: Some("updated after disconnect".into()),
                    ..Default::default()
                },
            );
            tokio::pin!(update_fut);
            tokio::select! {
                biased;
                res = &mut update_fut => {
                    res.unwrap();
                }
                _ = tokio::task::yield_now() => {}
            }
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let current = daemon.supervisor.get_vm(&vm.config.name).unwrap();
            if current.config.description == "updated after disconnect" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "update_vm did not finish in background after caller future was dropped"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let disk_vms = velovm_core::store::RegistryStore::new(dir.path())
            .unwrap()
            .load_all_vms()
            .unwrap();
        assert_eq!(disk_vms.len(), 1);
        assert_eq!(disk_vms[0].description, "updated after disconnect");

        // Start delete_vm and drop its caller future after one poll.
        {
            let delete_fut = daemon.supervisor.delete_vm(&vm.config.name, false);
            tokio::pin!(delete_fut);
            tokio::select! {
                biased;
                res = &mut delete_fut => {
                    res.unwrap();
                }
                _ = tokio::task::yield_now() => {}
            }
        }

        let del_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if daemon.supervisor.list_vms().is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < del_deadline,
                "delete_vm left VM stranded with deleting=true after caller future dropped"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!dir.path().join("vms").join(vm.config.id.to_string()).join("vm.toml").exists());

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn legacy_vm_with_relative_disk_whitespace_and_overlapping_ports_allows_metadata_updates() {
        let dir = tempfile::tempdir().unwrap();
        let vm_id = uuid::Uuid::new_v4();
        let vm_dir = dir.path().join("vms").join(vm_id.to_string());
        fs::create_dir_all(&vm_dir).unwrap();

        let mut legacy_vm = velovm_core::models::VmConfig::from_preset(
            "LegacyVm",
            VmPreset::Custom,
            &vm_dir,
        );
        legacy_vm.id = vm_id;
        legacy_vm.name = "  LegacyVm  ".into();
        legacy_vm.disks.push(velovm_core::models::DiskAttachment {
            lun: 0,
            kind: DiskKind::HardDisk,
            path: std::path::PathBuf::from("relative/legacy.img"),
            read_only: false,
            format: DiskFormat::Raw,
        });
        legacy_vm.nics[0].mac_address = "  00-15-5D-11-22-33  ".into();
        legacy_vm.nics[0].backend = NicBackendConfig::Consomme {
            cidr: None,
            port_forwards: vec![PortForward {
                protocol: IpProto::Tcp,
                host_addr: "127.0.0.1".into(),
                host_port: 8080,
                guest_port: 80,
            }],
            internal_dns: true,
            allow_host_local_access: false,
        };
        legacy_vm.nics.push(velovm_core::models::NicAttachment {
            id: uuid::Uuid::new_v4(),
            name: "nat1".into(),
            mac_address: "00-15-5D-11-22-34".into(),
            backend: NicBackendConfig::Consomme {
                cidr: None,
                port_forwards: vec![PortForward {
                    protocol: IpProto::Tcp,
                    host_addr: "0.0.0.0".into(),
                    host_port: 8080,
                    guest_port: 81,
                }],
                internal_dns: true,
                allow_host_local_access: false,
            },
        });
        let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
        legacy_vm.name = "LegacyVm".into();
        legacy_vm.nics[0].mac_address = "00-15-5D-11-22-33".into();
        store.save_vm_with_options(&legacy_vm, false, false).unwrap();
        let raw_toml = fs::read_to_string(store.vm_config_path(vm_id))
            .unwrap()
            .replacen("name = \"LegacyVm\"", "name = \"  LegacyVm  \"", 1)
            .replacen(
                "mac_address = \"00-15-5D-11-22-33\"",
                "mac_address = \"  00-15-5D-11-22-33  \"",
                1,
            )
            .replacen("host_addr = \"127.0.0.1\"", "host_addr = \" 127.0.0.1 \"", 1);
        fs::write(store.vm_config_path(vm_id), raw_toml).unwrap();

        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        // Loaded config is normalized at startup.
        let loaded: VmSummary = client
            .get(format!("{}/api/v1/vms/{vm_id}", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(loaded.config.name, "LegacyVm");
        assert_eq!(loaded.config.nics[0].mac_address, "00-15-5D-11-22-33");

        // Description-only update succeeds despite legacy relative disk & overlapping ports.
        let updated_resp = client
            .put(format!("{}/api/v1/vms/{vm_id}", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&UpdateVmPayload {
                description: Some("metadata update ok".into()),
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(updated_resp.status(), 200);
        let updated: VmSummary = updated_resp.json().await.unwrap();
        assert_eq!(updated.config.description, "metadata update ok");

        // Explicitly updating disks with a relative path is still rejected.
        let bad_disk_update = client
            .put(format!("{}/api/v1/vms/{vm_id}", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&UpdateVmPayload {
                disks: Some(loaded.config.disks),
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(bad_disk_update.status(), 400);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn patch_host_config_is_atomic_and_inspect_non_vhdx_ignores_missing_img_bin() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let missing_img_bin = dir.path().join("missing-openvmm-img");
        let firmware = dir.path().join("MSVM.fd");
        fs::write(&firmware, b"fw").unwrap();

        let mut init_cfg: velovm_core::models::HostConfig = client
            .get(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        init_cfg.openvmm_img_bin = Some(missing_img_bin.clone());
        init_cfg.uefi_firmware_path = Some(firmware.clone());
        let put_resp = client
            .put(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&init_cfg)
            .send()
            .await
            .unwrap();
        assert_eq!(put_resp.status(), 200);

        // PATCH updates only openvmm_bin without touching openvmm_img_bin or uefi_firmware_path.
        let custom_openvmm = dir.path().join("custom-openvmm");
        let patch_resp = client
            .patch(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&velovm_core::models::HostConfigPatch {
                openvmm_bin: Some(Some(custom_openvmm.clone())),
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(patch_resp.status(), 200);
        let patched: velovm_core::models::HostConfig = patch_resp.json().await.unwrap();
        assert_eq!(patched.openvmm_bin, Some(custom_openvmm));
        assert_eq!(patched.openvmm_img_bin, Some(missing_img_bin));
        assert_eq!(patched.uefi_firmware_path, Some(firmware));

        // Inspecting raw/ISO succeeds even though configured openvmm_img_bin does not exist.
        let raw_img = dir.path().join("guest.img");
        fs::write(&raw_img, vec![0u8; 4096]).unwrap();
        let raw_resp = client
            .post(format!("{}/api/v1/disks/inspect", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&velovm_core::models::InspectDiskPayload { path: raw_img })
            .send()
            .await
            .unwrap();
        assert_eq!(raw_resp.status(), 200);
        let raw_info: velovm_core::models::DiskInfo = raw_resp.json().await.unwrap();
        assert_eq!(raw_info.format, "raw");

        let iso_file = dir.path().join("disc.iso");
        fs::write(&iso_file, vec![0u8; 4096]).unwrap();
        let iso_resp = client
            .post(format!("{}/api/v1/disks/inspect", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&velovm_core::models::InspectDiskPayload { path: iso_file })
            .send()
            .await
            .unwrap();
        assert_eq!(iso_resp.status(), 200);
        let iso_info: velovm_core::models::DiskInfo = iso_resp.json().await.unwrap();
        assert_eq!(iso_info.format, "iso");

        // Inspecting `.vhdx` fails clearly with the configured missing openvmm-img error.
        let vhdx_file = dir.path().join("disk.vhdx");
        fs::write(&vhdx_file, b"vhdxfile").unwrap();
        let vhdx_resp = client
            .post(format!("{}/api/v1/disks/inspect", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&velovm_core::models::InspectDiskPayload { path: vhdx_file })
            .send()
            .await
            .unwrap();
        assert_eq!(vhdx_resp.status(), 400);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn list_vms_returns_atomic_event_revision_header() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let created: VmSummary = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "RevHeader-VM".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let vms_resp = client
            .get(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(vms_resp.status(), 200);
        let hdr_rev: u64 = vms_resp
            .headers()
            .get(velovm_core::models::EVENT_REVISION_HEADER)
            .expect("missing x-velovm-event-revision header")
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(hdr_rev, created.runtime.revision);

        let base_url = daemon.base_url.clone();
        let token = daemon.token.clone();
        let (api_vms, api_rev) = tokio::task::spawn_blocking(move || {
            velovm_core::api_client::ApiClient::new(&base_url, &token)?.list_vms_with_revision()
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(api_vms.len(), 1);
        assert_eq!(api_rev, Some(created.runtime.revision));

        daemon.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_default_runtime_dir_trailing_slash_gets_0700_and_worker_umask_is_0077() {
        use std::os::unix::fs::PermissionsExt;

        let dir = short_tempdir();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let fake_uefi = dir.path().join("MSVM.fd");
        fs::write(&fake_uefi, b"uefi").unwrap();

        // 1. Default runtime_dir specified with a trailing slash (pre-existing 0o755) must still get 0o700.
        let default_rt = velovm_core::models::default_runtime_dir(dir.path());
        fs::create_dir_all(&default_rt).unwrap();
        fs::set_permissions(&default_rt, fs::Permissions::from_mode(0o755)).unwrap();

        let exit_script = dir.path().join("exit-script.sh");
        fs::write(&exit_script, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&exit_script, fs::Permissions::from_mode(0o755)).unwrap();

        let mut cfg: velovm_core::models::HostConfig = client
            .get(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        cfg.runtime_dir = std::path::PathBuf::from(format!("{}/", default_rt.display()));
        cfg.openvmm_bin = Some(exit_script);
        cfg.uefi_firmware_path = Some(fake_uefi);
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

        let vm_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "Umask-VM".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(vm_resp.status(), 201);

        let _ = client
            .post(format!("{}/api/v1/vms/Umask-VM/start", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        let default_mode = fs::metadata(&default_rt).unwrap().permissions().mode() & 0o777;
        assert_eq!(default_mode, 0o700);

        // 2. Custom 0o755 runtime_dir keeps 0o755, while the spawned child creates files with umask 0o077.
        let custom_rt = dir.path().join("custom-umask-rt");
        fs::create_dir_all(&custom_rt).unwrap();
        fs::set_permissions(&custom_rt, fs::Permissions::from_mode(0o755)).unwrap();
        let probe_file = custom_rt.join("worker-created.probe");
        let probe_script = dir.path().join("probe-script.sh");
        fs::write(
            &probe_script,
            format!("#!/bin/sh\n: > '{}'\nexit 1\n", probe_file.display()),
        )
        .unwrap();
        fs::set_permissions(&probe_script, fs::Permissions::from_mode(0o755)).unwrap();

        cfg.runtime_dir = custom_rt.clone();
        cfg.openvmm_bin = Some(probe_script);
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

        let _ = client
            .post(format!("{}/api/v1/vms/Umask-VM/start", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        let custom_mode = fs::metadata(&custom_rt).unwrap().permissions().mode() & 0o777;
        assert_eq!(custom_mode, 0o755);
        let probe_mode = fs::metadata(&probe_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            probe_mode & 0o077,
            0,
            "child worker must run with umask 0o077 (got {probe_mode:o})"
        );

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn patch_host_config_empty_preserves_port0_listener_and_handles_unknown_null_empty_absent()
    {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = velovm_core::store::RegistryStore::new(dir.path()).unwrap();
            let mut cfg = store.load_or_init_host_config().unwrap();
            cfg.listen_addr = "127.0.0.1:0".into();
            store.save_host_config(&cfg).unwrap();
        }
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        // Daemon config starts with listen_addr = "127.0.0.1:0" on an ephemeral bound port.
        let initial: velovm_core::models::HostConfig = client
            .get(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(initial.listen_addr, "127.0.0.1:0");

        let custom_openvmm = dir.path().join("bin-openvmm");
        let custom_viewer = dir.path().join("bin-viewer");
        let custom_img = dir.path().join("bin-img");
        let seed_resp = client
            .patch(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&serde_json::json!({
                "openvmm_bin": custom_openvmm,
                "fastdesk_viewer_bin": custom_viewer,
                "openvmm_img_bin": custom_img,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(seed_resp.status(), 200);

        // 1. Empty PATCH {} must NOT rebind the listener when listen_addr is "127.0.0.1:0".
        let empty_resp = client
            .patch(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(empty_resp.status(), 200);
        // Daemon must still be reachable at the exact same base_url (listener did not move).
        let health = client
            .get(format!("{}/api/v1/health", daemon.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), 200);
        let after_empty: velovm_core::models::HostConfig = empty_resp.json().await.unwrap();
        assert_eq!(after_empty.openvmm_bin.as_ref(), Some(&custom_openvmm));
        assert_eq!(
            after_empty.fastdesk_viewer_bin.as_ref(),
            Some(&custom_viewer)
        );
        assert_eq!(after_empty.openvmm_img_bin.as_ref(), Some(&custom_img));

        // 2. Unknown fields must be rejected (422 Unprocessable Entity), not silently ignored.
        let unknown_resp = client
            .patch(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&serde_json::json!({ "runtime_dir": "/tmp/ignored" }))
            .send()
            .await
            .unwrap();
        assert_eq!(unknown_resp.status(), 422);

        // 3. `null` clears openvmm_bin, `""` clears fastdesk_viewer_bin, absent preserves openvmm_img_bin.
        let nullable_resp = client
            .patch(format!("{}/api/v1/host/config", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&serde_json::json!({
                "openvmm_bin": null,
                "fastdesk_viewer_bin": "",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(nullable_resp.status(), 200);
        let cleared: velovm_core::models::HostConfig = nullable_resp.json().await.unwrap();
        assert_eq!(cleared.openvmm_bin, None);
        assert_eq!(cleared.fastdesk_viewer_bin, None);
        assert_eq!(cleared.openvmm_img_bin, Some(custom_img));

        daemon.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_rollback_exposes_orphaned_reservations_over_http_and_clears_on_stop() {
        use velovm_core::ttrpc::{TtrpcStatus, encode_frame, read_frame};
        use prost::Message;
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        #[derive(Clone, PartialEq, prost::Message)]
        struct TestWireResponse {
            #[prost(oneof = "test_wire_response::Result", tags = "1, 2")]
            result: Option<test_wire_response::Result>,
        }
        mod test_wire_response {
            #[derive(Clone, PartialEq, prost::Oneof)]
            pub enum Result {
                #[prost(message, tag = "1")]
                Status(velovm_core::ttrpc::TtrpcStatus),
                #[prost(bytes, tag = "2")]
                Payload(Vec<u8>),
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();
        let shared_disk = dir.path().join("http-orphan.img");
        fs::write(&shared_disk, vec![0u8; 4096]).unwrap();

        let created: VmSummary = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "HttpOrphan-VM".into(),
                preset: velovm_core::models::VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let vm_id = created.config.id;
        let nic_id = created.config.nics[0].id;

        let sock_path = dir.path().join("orphan-http.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let _server = std::thread::spawn(move || {
            for idx in 0..4 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let Ok((stream_id, _, _)) = read_frame(&mut stream) else {
                    break;
                };
                let resp = if idx % 2 == 0 {
                    TestWireResponse {
                        result: Some(test_wire_response::Result::Payload(Vec::new())),
                    }
                } else {
                    TestWireResponse {
                        result: Some(test_wire_response::Result::Status(TtrpcStatus {
                            code: 13,
                            message: "injected rollback failure".into(),
                            details: Vec::new(),
                        })),
                    }
                };
                if let Ok(frame) = encode_frame(stream_id, 2, &resp.encode_to_vec()) {
                    let _ = stream.write_all(&frame);
                    let _ = stream.flush();
                }
            }
        });

        daemon
            .supervisor
            .set_vm_running_socket_for_test(vm_id, sock_path);
        let vm_dir = velovm_core::store::RegistryStore::open(dir.path()).vm_dir(vm_id);
        struct RestorePerm(std::path::PathBuf);
        impl Drop for RestorePerm {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let restore = RestorePerm(vm_dir.clone());
        fs::set_permissions(&vm_dir, fs::Permissions::from_mode(0o555)).unwrap();

        if fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(vm_dir.join(".probe"))
            .is_err()
        {
            let attach_resp = client
                .post(format!("{}/api/v1/vms/{vm_id}/disks", daemon.base_url))
                .bearer_auth(&daemon.token)
                .json(&velovm_core::models::AttachDiskPayload {
                    lun: Some(1),
                    path: shared_disk.clone(),
                    read_only: false,
                    format: Some(DiskFormat::Raw),
                })
                .send()
                .await
                .unwrap();
            assert!(!attach_resp.status().is_success());

            let pf = PortForward {
                protocol: IpProto::Tcp,
                host_addr: "127.0.0.1".into(),
                host_port: 19122,
                guest_port: 22,
            };
            let pf_resp = client
                .post(format!(
                    "{}/api/v1/vms/{vm_id}/nics/{nic_id}/ports",
                    daemon.base_url
                ))
                .bearer_auth(&daemon.token)
                .json(&velovm_core::models::PortForwardPayload { port: pf.clone() })
                .send()
                .await
                .unwrap();
            assert_eq!(pf_resp.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);

            let summary: VmSummary = client
                .get(format!("{}/api/v1/vms/{vm_id}", daemon.base_url))
                .bearer_auth(&daemon.token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(summary.runtime.orphaned_disks.len(), 1);
            assert_eq!(summary.runtime.orphaned_disks[0].path, shared_disk);
            assert_eq!(summary.runtime.orphaned_port_forwards.len(), 1);
            assert_eq!(summary.runtime.orphaned_port_forwards[0].forward, pf);

            drop(restore);
            let stopped: VmSummary = client
                .post(format!("{}/api/v1/vms/{vm_id}/stop", daemon.base_url))
                .bearer_auth(&daemon.token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert!(stopped.runtime.orphaned_disks.is_empty());
            assert!(stopped.runtime.orphaned_port_forwards.is_empty());
        } else {
            drop(restore);
        }

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn attach_disk_rejects_iso_with_non_iso_formats_on_offline_vm() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let iso_path = dir.path().join("installer.iso");
        fs::write(&iso_path, vec![0u8; 4096]).unwrap();

        let create_resp: VmSummary = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "IsoFormatGuard".into(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(1024),
                ..Default::default()
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let vm_id = create_resp.config.id;

        for bad_format in [DiskFormat::Vhdx, DiskFormat::Vhd, DiskFormat::Raw] {
            let resp = client
                .post(format!("{}/api/v1/vms/{vm_id}/disks", daemon.base_url))
                .bearer_auth(&daemon.token)
                .json(&velovm_core::models::AttachDiskPayload {
                    path: iso_path.clone(),
                    lun: Some(0),
                    read_only: false,
                    format: Some(bad_format),
                })
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                reqwest::StatusCode::BAD_REQUEST,
                "expected 400 Bad Request when attaching .iso as {bad_format:?}"
            );
        }

        let summary: VmSummary = client
            .get(format!("{}/api/v1/vms/{vm_id}", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(summary.config.disks.is_empty());

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_vm_returns_bad_request_when_acpi_and_force_are_both_set() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let resp = client
            .post(format!("{}/api/v1/vms/nonexistent/shutdown", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&velovm_core::models::ShutdownVmPayload {
                acpi: true,
                force: true,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn virtual_switch_crud_endpoints_validate_reject_builtin_and_detect_vm_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        // 1. GET /api/v1/host/net-adapters returns 200 OK JSON array
        let adapters_resp = client
            .get(format!("{}/api/v1/host/net-adapters", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(adapters_resp.status(), reqwest::StatusCode::OK);
        let _adapters: Vec<HostNetAdapterInfo> = adapters_resp.json().await.unwrap();

        // 2. POST /api/v1/host/switches validation errors -> 400 Bad Request
        let empty_name_resp = client
            .post(format!("{}/api/v1/host/switches", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateSwitchPayload {
                name: "   ".to_string(),
                kind: "internal".to_string(),
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(empty_name_resp.status(), reqwest::StatusCode::BAD_REQUEST);

        let missing_adapter_resp = client
            .post(format!("{}/api/v1/host/switches", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateSwitchPayload {
                name: "BadExt".to_string(),
                kind: "external".to_string(),
                net_adapter_name: None,
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(
            missing_adapter_resp.status(),
            reqwest::StatusCode::BAD_REQUEST
        );

        // 3. DELETE built-in switches -> 400 Bad Request; missing switch -> 404 Not Found
        for builtin in ["consomme", "c08cb7b8-9b3c-408e-8e30-5e16a3aeb444"] {
            let del_builtin = client
                .delete(format!("{}/api/v1/host/switches/{builtin}", daemon.base_url))
                .bearer_auth(&daemon.token)
                .send()
                .await
                .unwrap();
            assert_eq!(del_builtin.status(), reqwest::StatusCode::BAD_REQUEST);
        }

        let del_missing = client
            .delete(format!(
                "{}/api/v1/host/switches/nonexistent-switch",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(del_missing.status(), reqwest::StatusCode::NOT_FOUND);

        // 4. DELETE switch attached to a VM -> 409 Conflict
        #[cfg(target_os = "linux")]
        let (target_switch, nic_backend) = (
            "br-conflict",
            velovm_core::models::NicBackendConfig::Bridge {
                name: "br-conflict".to_string(),
            },
        );
        #[cfg(not(target_os = "linux"))]
        let (target_switch, nic_backend) = (
            "11111111-2222-3333-4444-555555555555",
            velovm_core::models::NicBackendConfig::VmSwitch {
                switch_id: "11111111-2222-3333-4444-555555555555".to_string(),
                switch_name: Some("LabSwitch".to_string()),
            },
        );

        let create_vm_resp = client
            .post(format!("{}/api/v1/vms", daemon.base_url))
            .bearer_auth(&daemon.token)
            .json(&CreateVmPayload {
                name: "SwitchConflictVm".to_string(),
                preset: VmPreset::Custom,
                vcpus: Some(2),
                memory_mb: Some(512),
                nics: Some(vec![velovm_core::models::NicAttachment {
                    id: Uuid::new_v4(),
                    name: "Test NIC".to_string(),
                    mac_address: "00-15-5D-11-22-33".to_string(),
                    backend: nic_backend,
                }]),
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(create_vm_resp.status(), reqwest::StatusCode::CREATED);

        let conflict_resp = client
            .delete(format!(
                "{}/api/v1/host/switches/{target_switch}",
                daemon.base_url
            ))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(conflict_resp.status(), reqwest::StatusCode::CONFLICT);

        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn sse_events_stream_delivers_switches_updated_event() {
        use futures_util::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let daemon = TestDaemonHandle::spawn(dir.path().to_path_buf())
            .await
            .unwrap();
        let client = test_http_client();

        let resp = client
            .get(format!("{}/api/v1/events", daemon.base_url))
            .bearer_auth(&daemon.token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let mut stream = resp.bytes_stream();
        // Read initial Resync event
        let first_chunk = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .expect("initial SSE resync")
            .unwrap()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&first_chunk).contains("resync"),
            "unexpected initial SSE chunk: {}",
            String::from_utf8_lossy(&first_chunk)
        );

        daemon.supervisor.emit(DaemonEvent::SwitchesUpdated);

        let update_chunk = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .expect("SwitchesUpdated SSE event")
            .unwrap()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&update_chunk).contains("switches_updated"),
            "expected switches_updated SSE event, got: {}",
            String::from_utf8_lossy(&update_chunk)
        );

        daemon.shutdown().await;
    }
}
