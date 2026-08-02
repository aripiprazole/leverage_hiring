mod oci;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use barbirolli::{Barbirolli, LifecycleError, StorageError, VmId, VmInput, VmStatus};
use serde::Serialize;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

#[derive(Clone)]
struct AppState {
    manager: Barbirolli,
}

pub fn router(manager: Barbirolli) -> Router {
    Router::new()
        .route("/vms", get(list_vms).post(create_vm))
        .route("/vms/{id}", get(vm).delete(delete_vm))
        .route("/vms/{id}/status", get(vm_status))
        .route("/vms/{id}/start", post(start_vm))
        .route("/vms/{id}/shutdown", post(shutdown_vm))
        .route("/run", post(oci::run))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(AppState { manager })
        .layer(Extension(oci::OciFetcher::default()))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
}

#[derive(Serialize)]
struct StatusResponse {
    id: VmId,
    status: VmStatus,
}

#[tracing::instrument(skip(state))]
async fn list_vms(State(state): State<AppState>) -> Json<Vec<barbirolli::VmSummary>> {
    tracing::info!("list vms");
    Json(state.manager.list().await)
}

#[tracing::instrument(skip(state))]
async fn vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<barbirolli::VmSummary>, ApiError> {
    tracing::info!("get vm");
    let id = parse_vm_id(&id)?;
    Ok(Json(
        state.manager.vm_mut(id).map_err(ApiError::from)?.summary(),
    ))
}

#[tracing::instrument(skip(state))]
async fn create_vm(
    State(state): State<AppState>,
    input: Result<Json<VmInput>, JsonRejection>,
) -> Result<(StatusCode, Json<StatusResponse>), ApiError> {
    tracing::info!("create vm");
    let Json(input) = input.map_err(|error| ApiError::UnprocessableEntity(error.body_text()))?;
    let id = state.manager.create(input).await.map_err(ApiError::from)?;
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
    tracing::info!("vm status");
    let id = parse_vm_id(&id)?;
    let summary = state.manager.vm_mut(id).map_err(ApiError::from)?.summary();
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
    tracing::info!("start vm");
    let id = parse_vm_id(&id)?;
    let mut vm = state.manager.vm_mut(id).map_err(ApiError::from)?;
    vm.start(&state.manager).await.map_err(ApiError::from)?;
    let summary = vm.summary();
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
    tracing::info!("shutdown vm");
    let id = parse_vm_id(&id)?;
    let mut vm = state.manager.vm_mut(id).map_err(ApiError::from)?;
    vm.shutdown(&state.manager).await.map_err(ApiError::from)?;
    let summary = vm.summary();
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
    tracing::info!("delete vm");
    let id = parse_vm_id(&id)?;
    state.manager.delete(id).await.map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument]
async fn not_found() -> ApiError {
    ApiError::NotFound("route not found".to_owned())
}

#[tracing::instrument]
async fn method_not_allowed() -> ApiError {
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
        match error {
            oci::OciError::InvalidInput(message) => Self::UnprocessableEntity(message),
            error => {
                tracing::error!(%error, "OCI request failed");
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
                StorageError::SocketDirectory
                | StorageError::CreatingDirectory
                | StorageError::Io { .. }
                | StorageError::InvalidConfig { .. },
            )
            | LifecycleError::Shutdown(_)
            | LifecycleError::Warmup(_) => {
                tracing::error!(%error, "Elhone request failed");
                Self::InternalServerError(message)
            }
            #[cfg(target_os = "linux")]
            LifecycleError::Storage(StorageError::SshAccess(_)) => {
                tracing::error!(%error, "Elhone request failed");
                Self::InternalServerError(message)
            }
            #[cfg(not(target_os = "linux"))]
            LifecycleError::UnsupportedPlatform => {
                tracing::error!(%error, "Elhone request failed");
                Self::InternalServerError(message)
            }
            #[cfg(target_os = "linux")]
            LifecycleError::Vm(_) => {
                tracing::error!(%error, "Elhone request failed");
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
    use barbirolli::{NetworkSpec, Port, PortBinding, VmId, VmInput, VmStatus, VmSummary};
    use serde_json::json;

    #[test]
    fn vm_input_rejects_artifact_selection() {
        for (field, value) in [("kernel", "vmlinux"), ("rootfs", "alpine.ext4")] {
            let mut input = json!({
                "vcpu_count": 1
            });
            input[field] = value.into();

            let error = serde_json::from_value::<VmInput>(input)
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
        let error = serde_json::from_value::<VmInput>(json!({
            "user": "alice",
            "vcpu_count": 1
        }))
        .expect_err("user must not be accepted");

        assert!(error.to_string().contains("unknown field `user`"));
    }

    #[test]
    fn vm_input_accepts_typed_bindings_and_defaults_to_none() {
        let without_bindings = serde_json::from_value::<VmInput>(json!({
            "vcpu_count": 1
        }))
        .expect("bindings should be optional");
        assert!(without_bindings.bindings.is_empty());

        let with_bindings = serde_json::from_value::<VmInput>(json!({
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

            let error = serde_json::from_value::<VmInput>(json!({
                "vcpu_count": 1,
                "bindings": [binding]
            }))
            .expect_err("port zero must be rejected");
            assert!(error.to_string().contains("invalid network port"));
        }
    }

    #[test]
    fn vm_input_rejects_port_bindings() {
        let error = serde_json::from_value::<VmInput>(json!({
            "vcpu_count": 1,
            "port_bindings": []
        }))
        .expect_err("port_bindings must not be accepted");

        assert!(error.to_string().contains("unknown field `port_bindings`"));
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
