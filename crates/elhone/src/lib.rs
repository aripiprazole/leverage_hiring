mod oci;

use std::{io, path::PathBuf, time::Duration};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use barbirolli::{
    AuthorizedKey, Barbirolli, LifecycleError, PortBinding, Rootfs, StorageError, VcpuCount, VmId,
    VmInput, VmStatus,
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

const LOG_READ_BUFFER_SIZE: usize = 8 * 1024;
const LOG_FOLLOW_INTERVAL: Duration = Duration::from_millis(100);

/// Installs the ring Rustls crypto provider when no process-wide provider exists.
///
/// # Panics
///
/// Panics if another caller installs a process-wide crypto provider after the
/// initial check but before ring is installed.
pub fn install_ring_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let () = rustls::crypto::ring::default_provider()
            .install_default()
            .unwrap();
    }
}

#[derive(Clone)]
struct AppState {
    manager: Barbirolli,
    standard_rootfs: Rootfs,
    oci_store: oci::OciStore,
}

pub fn router(manager: Barbirolli, standard_rootfs: Rootfs) -> Router {
    Router::new()
        .route("/vms", get(list_vms).post(create_vm))
        .route("/vms/{id}", get(vm).delete(delete_vm))
        .route("/vms/{id}/logs", get(vm_logs))
        .route("/vms/{id}/status", get(vm_status))
        .route("/vms/{id}/start", post(start_vm))
        .route("/vms/{id}/shutdown", post(shutdown_vm))
        .route("/oci/pull", post(oci::pull))
        .route("/oci/run", post(oci::run))
        .route("/oci/stop", post(oci::stop))
        .route("/oci/rm", post(oci::rm))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(AppState {
            manager,
            standard_rootfs,
            oci_store: oci::OciStore::default(),
        })
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(|request: &Request<Body>, _span: &tracing::Span| {
                    tracing::trace!(
                        method = %request.method(),
                        uri = %request.uri(),
                        "elhone starts to handle the http request"
                    );
                })
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
}

#[derive(Serialize)]
struct StatusResponse {
    id: VmId,
    status: VmStatus,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct VmLogsQuery {
    follow: bool,
}

struct VmLogStreamState {
    file: tokio::fs::File,
    path: PathBuf,
    follow: bool,
    remaining: Option<u64>,
    done: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateVmRequest {
    vcpu_count: VcpuCount,
    #[serde(default)]
    authorized_keys: Vec<AuthorizedKey>,
    #[serde(default)]
    bindings: Vec<PortBinding>,
}

impl CreateVmRequest {
    fn into_vm_input(self, rootfs: Rootfs) -> VmInput {
        VmInput {
            rootfs,
            provision_ssh_keys: true,
            vcpu_count: self.vcpu_count,
            authorized_keys: self.authorized_keys,
            bindings: self.bindings,
        }
    }
}

#[tracing::instrument(skip(state))]
async fn list_vms(State(state): State<AppState>) -> Json<Vec<barbirolli::VmSummary>> {
    tracing::info!("elhone starts to list the VMs");
    let vms = state.manager.list().await;
    tracing::info!(vm_count = vms.len(), "elhone listed the VMs");
    Json(vms)
}

#[tracing::instrument(skip(state))]
async fn vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<barbirolli::VmSummary>, ApiError> {
    tracing::info!(vm_id = %id, "elhone starts to get the VM");
    let id = parse_vm_id(&id)?;
    let summary = state.manager.vm_mut(id).map_err(ApiError::from)?.summary();
    tracing::info!(%id, "elhone found the VM");
    Ok(Json(summary))
}

#[tracing::instrument(skip(state, query))]
async fn vm_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<VmLogsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    tracing::info!(vm_id = %id, "elhone starts to get the VM serial log");
    let id = parse_vm_id(&id)?;
    let Query(query) = query.map_err(|error| ApiError::UnprocessableEntity(error.body_text()))?;
    let path = {
        let vm = state.manager.vm(id).map_err(ApiError::from)?;
        vm.spec().serial_log()
    };
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|error| vm_log_io_error(id, "open", &path, &error))?;
    let len = file
        .metadata()
        .await
        .map_err(|error| vm_log_io_error(id, "inspect", &path, &error))?
        .len();
    tracing::info!(
        %id,
        path = %path.display(),
        follow = query.follow,
        byte_count = len,
        "elhone opened the VM serial log"
    );
    let stream = vm_log_stream(file, path, query.follow, len);
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if !query.follow {
        response.headers_mut().insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&len.to_string())
                .expect("a decimal content length is always a valid header value"),
        );
    }
    Ok(response)
}

fn vm_log_stream(
    file: tokio::fs::File,
    path: PathBuf,
    follow: bool,
    snapshot_length: u64,
) -> impl futures::Stream<Item = Result<Bytes, io::Error>> + Send + 'static {
    let state = VmLogStreamState {
        file,
        path,
        follow,
        remaining: (!follow).then_some(snapshot_length),
        done: false,
    };
    futures::stream::unfold(state, |mut state| async move {
        if state.done {
            return None;
        }
        loop {
            let read_size = state.remaining.map_or(LOG_READ_BUFFER_SIZE, |remaining| {
                usize::try_from(remaining)
                    .unwrap_or(LOG_READ_BUFFER_SIZE)
                    .min(LOG_READ_BUFFER_SIZE)
            });
            if read_size == 0 {
                return None;
            }
            let mut buffer = vec![0_u8; read_size];
            match state.file.read(&mut buffer).await {
                Ok(0) if state.follow => {
                    tokio::time::sleep(LOG_FOLLOW_INTERVAL).await;
                }
                Ok(0) => return None,
                Ok(read) => {
                    buffer.truncate(read);
                    if let Some(remaining) = state.remaining.as_mut() {
                        *remaining -= read as u64;
                    }
                    return Some((Ok(Bytes::from(buffer)), state));
                }
                Err(error) => {
                    tracing::error!(%error, path = %state.path.display(), "the VM log stream failed");
                    state.done = true;
                    return Some((Err(error), state));
                }
            }
        }
    })
}

fn vm_log_io_error(
    id: VmId,
    operation: &'static str,
    path: &std::path::Path,
    error: &io::Error,
) -> ApiError {
    tracing::error!(%error, %id, operation, path = %path.display(), "the VM log request failed");
    ApiError::InternalServerError(format!("failed to {operation} VM {id} serial log"))
}

#[tracing::instrument(skip(state))]
async fn create_vm(
    State(state): State<AppState>,
    input: Result<Json<CreateVmRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<StatusResponse>), ApiError> {
    tracing::info!("elhone starts to create the VM");
    let Json(input) = input.map_err(|error| ApiError::UnprocessableEntity(error.body_text()))?;
    let input = input.into_vm_input(state.standard_rootfs);
    let id = state.manager.create(input).await.map_err(ApiError::from)?;
    tracing::info!(%id, "elhone created the VM");
    Ok((
        StatusCode::CREATED,
        Json(StatusResponse {
            id,
            status: VmStatus::Discovered,
        }),
    ))
}

#[tracing::instrument(skip(state))]
async fn vm_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StatusResponse>, ApiError> {
    tracing::info!(vm_id = %id, "elhone starts to get the VM status");
    let id = parse_vm_id(&id)?;
    let summary = state.manager.vm_mut(id).map_err(ApiError::from)?.summary();
    tracing::info!(%id, status = ?summary.status, "elhone read the VM status");
    Ok(Json(StatusResponse {
        id,
        status: summary.status,
    }))
}

#[tracing::instrument(skip(state))]
async fn start_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StatusResponse>, ApiError> {
    tracing::info!(vm_id = %id, "elhone starts VM startup");
    let id = parse_vm_id(&id)?;
    let mut vm = state.manager.vm_mut(id).map_err(ApiError::from)?;
    vm.start(&state.manager).await.map_err(ApiError::from)?;
    let summary = vm.summary();
    tracing::info!(%id, "elhone started the VM");
    Ok(Json(StatusResponse {
        id,
        status: summary.status,
    }))
}

#[tracing::instrument(skip(state))]
async fn shutdown_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StatusResponse>, ApiError> {
    tracing::info!(vm_id = %id, "elhone starts VM shutdown");
    let id = parse_vm_id(&id)?;
    let mut vm = state.manager.vm_mut(id).map_err(ApiError::from)?;
    vm.shutdown(&state.manager).await.map_err(ApiError::from)?;
    let summary = vm.summary();
    tracing::info!(%id, "elhone stopped the VM");
    Ok(Json(StatusResponse {
        id,
        status: summary.status,
    }))
}

#[tracing::instrument(skip(state))]
async fn delete_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    tracing::info!(vm_id = %id, "elhone starts to delete the VM");
    let id = parse_vm_id(&id)?;
    state.manager.delete(id).await.map_err(ApiError::from)?;
    tracing::info!(%id, "elhone deleted the VM");
    Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument]
async fn not_found() -> ApiError {
    tracing::info!("elhone starts to handle an unknown route");
    ApiError::NotFound("route not found".to_owned())
}

#[tracing::instrument]
async fn method_not_allowed() -> ApiError {
    tracing::info!("elhone starts method-not-allowed handling");
    ApiError::MethodNotAllowed
}

#[derive(Debug, thiserror::Error)]
enum ApiError {
    #[error("{0}")]
    NotFound(String),
    #[error("method not allowed")]
    MethodNotAllowed,
    #[error("{0}")]
    UnprocessableEntity(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    BadGateway(String),
    #[error("{0}")]
    InternalServerError(String),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::UnprocessableEntity(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::BadGateway(_) => StatusCode::BAD_GATEWAY,
            Self::InternalServerError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<oci::OciError> for ApiError {
    fn from(error: oci::OciError) -> Self {
        if error.is_local_failure() {
            tracing::error!(%error, "the OCI artifact build failed");
            return Self::InternalServerError(error.to_string());
        }
        match error {
            oci::OciError::InvalidInput(message) => Self::UnprocessableEntity(message),
            error => {
                tracing::error!(%error, "the OCI request failed");
                Self::BadGateway(error.to_string())
            }
        }
    }
}

impl From<LifecycleError> for ApiError {
    fn from(error: LifecycleError) -> Self {
        let message = error.to_string();
        match &error {
            LifecycleError::NotFound(_) => Self::NotFound(message),
            LifecycleError::Storage(StorageError::InvalidInput(_) | StorageError::IdsExhausted) => {
                Self::UnprocessableEntity(message)
            }
            LifecycleError::Draining
            | LifecycleError::CapacityReached { .. }
            | LifecycleError::InvalidTransition { .. } => Self::Conflict(message),
            LifecycleError::Storage(
                StorageError::CreatingDirectory
                | StorageError::Io { .. }
                | StorageError::InvalidConfig { .. },
            )
            | LifecycleError::Shutdown(_)
            | LifecycleError::Warmup(_) => {
                tracing::error!(%error, "the VM request failed");
                Self::InternalServerError(message)
            }
            #[cfg(target_os = "linux")]
            LifecycleError::Storage(StorageError::RootfsProvision(_)) => {
                tracing::error!(%error, "the VM request failed");
                Self::InternalServerError(message)
            }
            #[cfg(not(target_os = "linux"))]
            LifecycleError::UnsupportedPlatform => {
                tracing::error!(%error, "the VM request failed");
                Self::InternalServerError(message)
            }
            #[cfg(target_os = "linux")]
            LifecycleError::Vm(_) => {
                tracing::error!(%error, "the VM request failed");
                Self::InternalServerError(message)
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            error: String,
        }
        let status = self.status();
        let message = self.to_string();
        (status, Json(ErrorBody { error: message })).into_response()
    }
}

fn parse_vm_id(value: &str) -> Result<VmId, ApiError> {
    let raw = value
        .parse::<u16>()
        .map_err(|_| ApiError::UnprocessableEntity(format!("invalid VM ID {value:?}")))?;
    VmId::try_from(raw).map_err(|error| ApiError::UnprocessableEntity(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use axum::{extract::Query, http::Uri};
    use barbirolli::{NetworkSpec, Port, PortBinding, Rootfs, VmId, VmStatus, VmSummary};
    use futures::{StreamExt, TryStreamExt};
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    use super::{CreateVmRequest, VmLogsQuery, vm_log_stream};

    #[test]
    fn vm_logs_query_defaults_to_pull_and_rejects_invalid_input() {
        let Query(defaults) = Query::<VmLogsQuery>::try_from_uri(
            &"http://localhost/vms/0/logs"
                .parse::<Uri>()
                .expect("valid URI"),
        )
        .expect("an absent query should use defaults");
        assert!(!defaults.follow);

        let Query(attach) = Query::<VmLogsQuery>::try_from_uri(
            &"http://localhost/vms/0/logs?follow=true"
                .parse::<Uri>()
                .expect("valid URI"),
        )
        .expect("follow=true should deserialize");
        assert!(attach.follow);

        for query in ["follow=maybe", "unknown=true"] {
            let uri = format!("http://localhost/vms/0/logs?{query}")
                .parse::<Uri>()
                .expect("valid URI");
            assert!(Query::<VmLogsQuery>::try_from_uri(&uri).is_err());
        }
    }

    #[tokio::test]
    async fn vm_log_snapshot_stops_at_the_initial_file_length() {
        let temporary = tempfile::tempdir().expect("failed to create temporary directory");
        let path = temporary.path().join("serial.log");
        tokio::fs::write(&path, b"retained\n")
            .await
            .expect("failed to seed serial log");
        let file = tokio::fs::File::open(&path)
            .await
            .expect("failed to open serial log");
        let length = file
            .metadata()
            .await
            .expect("failed to inspect serial log")
            .len();
        let stream = vm_log_stream(file, path.clone(), false, length);

        let mut writer = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .expect("failed to open serial log writer");
        writer
            .write_all(b"after-snapshot\n")
            .await
            .expect("failed to append serial log");
        writer.flush().await.expect("failed to flush serial log");

        let chunks = stream
            .try_collect::<Vec<_>>()
            .await
            .expect("snapshot stream failed");
        assert_eq!(chunks.concat(), b"retained\n");
    }

    #[tokio::test]
    async fn vm_log_followers_are_independent_and_wait_for_appended_bytes() {
        let temporary = tempfile::tempdir().expect("failed to create temporary directory");
        let path = temporary.path().join("serial.log");
        tokio::fs::write(&path, b"retained\n")
            .await
            .expect("failed to seed serial log");
        let first_file = tokio::fs::File::open(&path)
            .await
            .expect("failed to open serial log");
        let second_file = tokio::fs::File::open(&path)
            .await
            .expect("failed to open second serial log reader");
        let first = vm_log_stream(first_file, path.clone(), true, 0);
        let second = vm_log_stream(second_file, path.clone(), true, 0);
        futures::pin_mut!(first, second);

        let first_retained = tokio::time::timeout(Duration::from_secs(1), first.next())
            .await
            .expect("retained output timed out")
            .expect("follow stream ended")
            .expect("follow stream failed");
        let second_retained = tokio::time::timeout(Duration::from_secs(1), second.next())
            .await
            .expect("second retained output timed out")
            .expect("second follow stream ended")
            .expect("second follow stream failed");
        assert_eq!(first_retained, b"retained\n"[..]);
        assert_eq!(second_retained, b"retained\n"[..]);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), first.next())
                .await
                .is_err(),
            "first follow stream must wait rather than end at EOF"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second.next())
                .await
                .is_err(),
            "second follow stream must wait rather than end at EOF"
        );

        let mut writer = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .expect("failed to open serial log writer");
        writer
            .write_all(b"appended\n")
            .await
            .expect("failed to append serial log");
        writer.flush().await.expect("failed to flush serial log");

        let first_appended = tokio::time::timeout(Duration::from_secs(1), first.next())
            .await
            .expect("appended output timed out")
            .expect("follow stream ended")
            .expect("follow stream failed");
        let second_appended = tokio::time::timeout(Duration::from_secs(1), second.next())
            .await
            .expect("second appended output timed out")
            .expect("second follow stream ended")
            .expect("second follow stream failed");
        assert_eq!(first_appended, b"appended\n"[..]);
        assert_eq!(second_appended, b"appended\n"[..]);
    }

    #[test]
    fn vm_input_rejects_artifact_selection() {
        for (field, value) in [("kernel", "vmlinux"), ("rootfs", "ubuntu-24.04.ext4")] {
            let mut input = json!({
                "vcpu_count": 1
            });
            input[field] = value.into();

            let error = serde_json::from_value::<CreateVmRequest>(input)
                .expect_err("artifact selection must not be accepted");
            assert!(
                error
                    .to_string()
                    .contains(&format!("unknown field `{field}`"))
            );
        }
    }

    #[test]
    fn vm_input_rejects_user() {
        let error = serde_json::from_value::<CreateVmRequest>(json!({
            "user": "alice",
            "vcpu_count": 1
        }))
        .expect_err("user must not be accepted");

        assert!(error.to_string().contains("unknown field `user`"));
    }

    #[test]
    fn vm_input_accepts_typed_bindings_and_defaults_to_none() {
        let without_bindings = serde_json::from_value::<CreateVmRequest>(json!({
            "vcpu_count": 1
        }))
        .expect("bindings should be optional");
        assert!(without_bindings.bindings.is_empty());

        let with_bindings = serde_json::from_value::<CreateVmRequest>(json!({
            "vcpu_count": 1,
            "bindings": [
                {
                    "internal": 22,
                    "external": 2222
                }
            ]
        }))
        .expect("valid bindings should deserialize");
        assert_eq!(
            with_bindings.bindings,
            vec![PortBinding {
                internal: Port::try_from(22).expect("valid internal port"),
                external: Port::try_from(2222).expect("valid external port"),
            }]
        );
    }

    #[test]
    fn vm_input_rejects_zero_bindings() {
        for field in ["internal", "external"] {
            let mut binding = json!({
                "internal": 22,
                "external": 2222
            });
            binding[field] = 0.into();

            let error = serde_json::from_value::<CreateVmRequest>(json!({
                "vcpu_count": 1,
                "bindings": [binding]
            }))
            .expect_err("port zero must be rejected");
            assert!(error.to_string().contains("invalid network port"));
        }
    }

    #[test]
    fn vm_input_rejects_port_bindings() {
        let error = serde_json::from_value::<CreateVmRequest>(json!({
            "vcpu_count": 1,
            "port_bindings": []
        }))
        .expect_err("port_bindings must not be accepted");

        assert!(error.to_string().contains("unknown field `port_bindings`"));
    }

    #[test]
    fn vm_input_receives_the_daemon_standard_rootfs() {
        let input = serde_json::from_value::<CreateVmRequest>(json!({
            "vcpu_count": 1
        }))
        .expect("valid VM input")
        .into_vm_input(Rootfs::from(PathBuf::from(
            "/var/lib/images/ubuntu-24.04.ext4",
        )));

        assert_eq!(
            input.rootfs,
            Rootfs::from(PathBuf::from("/var/lib/images/ubuntu-24.04.ext4"))
        );
    }

    #[test]
    fn vm_summary_serializes_network() {
        let id = VmId::try_from(0).expect("valid VM ID");
        let summary = VmSummary {
            id,
            status: VmStatus::Running,
            network: NetworkSpec::new(id).expect("valid network"),
            bindings: vec![PortBinding {
                internal: Port::try_from(22).expect("valid internal port"),
                external: Port::try_from(2222).expect("valid external port"),
            }],
        };

        assert_eq!(
            serde_json::to_value(summary).expect("summary should serialize"),
            json!({
                "id": 0,
                "status": "running",
                "bindings": [
                    {
                        "internal": 22,
                        "external": 2222
                    }
                ],
                "network": {
                    "vm_id": 0,
                    "tap": "fc-tap0",
                    "subnet": "172.16.0.0",
                    "host_ip": "172.16.0.1",
                    "guest_ip": "172.16.0.2",
                    "guest_mac": "06:00:ac:10:00:02"
                }
            })
        );
    }
}
