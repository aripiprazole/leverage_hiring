use std::{env, net::SocketAddr, sync::Arc};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State, rejection::JsonRejection},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use barbirolli::{Barbirolli, LifecycleError, StorageError, VmId, VmInput, VmStatus};
use serde::Serialize;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

#[derive(Clone)]
pub enum Auth {
    Bearer(Arc<str>),
    Local,
}

impl Auth {
    pub fn bearer(token: impl Into<Arc<str>>) -> Self {
        Self::Bearer(token.into())
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        if cfg!(feature = "local") {
            Ok(Self::Local)
        } else {
            let token = env::var("ELHONE_TOKEN")
                .map_err(|_| ConfigError::MissingVariable("ELHONE_TOKEN"))?;
            if token.is_empty() {
                Err(ConfigError::EmptyToken)
            } else {
                Ok(Self::bearer(token))
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("required environment variable {0} is not set")]
    MissingVariable(&'static str),
    #[error("ELHONE_TOKEN must not be empty")]
    EmptyToken,
    #[error("the local feature requires ELHONE_ADDR to be loopback, got {0}")]
    NonLoopbackLocal(SocketAddr),
}

pub fn validate_address(address: SocketAddr) -> Result<SocketAddr, ConfigError> {
    if cfg!(feature = "local") && !address.ip().is_loopback() {
        return Err(ConfigError::NonLoopbackLocal(address));
    }
    Ok(address)
}

#[derive(Clone)]
struct AppState {
    manager: Arc<Barbirolli>,
}

pub fn router(manager: Arc<Barbirolli>, auth: Auth) -> Router {
    let app = Router::new()
        .route("/vms", get(list_vms).post(create_vm))
        .route("/vms/{id}", delete(delete_vm))
        .route("/vms/{id}/status", get(vm_status))
        .route("/vms/{id}/start", post(start_vm))
        .route("/vms/{id}/shutdown", post(shutdown_vm))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(AppState { manager })
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        );

    if cfg!(feature = "local") {
        app
    } else {
        app.layer(middleware::from_fn_with_state(auth, authorize))
    }
}

async fn authorize(
    State(auth): State<Auth>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let authorized = match auth {
        Auth::Local => true,
        Auth::Bearer(expected) => request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|actual| actual == expected.as_ref()),
    };
    if authorized {
        Ok(next.run(request).await)
    } else {
        Err(ApiError::Forbidden)
    }
}

#[derive(Serialize)]
struct StatusResponse {
    id: VmId,
    status: VmStatus,
}

#[tracing::instrument(skip(state))]
async fn list_vms(State(state): State<AppState>) -> Json<Vec<barbirolli::VmSummary>> {
    Json(state.manager.list().await)
}

#[tracing::instrument(skip(state))]
async fn create_vm(
    State(state): State<AppState>,
    input: Result<Json<VmInput>, JsonRejection>,
) -> Result<(StatusCode, Json<StatusResponse>), ApiError> {
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
    let id = parse_vm_id(id)?;
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
    let id = parse_vm_id(id)?;
    let mut vm = state.manager.vm_mut(id).map_err(ApiError::from)?;
    Box::pin(vm.start(&state.manager))
        .await
        .map_err(ApiError::from)?;
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
    let id = parse_vm_id(id)?;
    let mut vm = state.manager.vm_mut(id).map_err(ApiError::from)?;
    Box::pin(vm.shutdown(&state.manager))
        .await
        .map_err(ApiError::from)?;
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
    let id = parse_vm_id(id)?;
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
    #[error("forbidden")]
    Forbidden,
    #[error("{0}")]
    NotFound(String),
    #[error("method not allowed")]
    MethodNotAllowed,
    #[error("{0}")]
    UnprocessableEntity(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    InternalServerError(String),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::UnprocessableEntity(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::InternalServerError(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
            LifecycleError::Draining | LifecycleError::InvalidTransition { .. } => {
                Self::Conflict(message)
            }
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
            #[cfg(feature = "linux")]
            LifecycleError::Storage(StorageError::SshAccess(_)) => {
                tracing::error!(%error, "Elhone request failed");
                Self::InternalServerError(message)
            }
            #[cfg(not(feature = "linux"))]
            LifecycleError::UnsupportedPlatform => {
                tracing::error!(%error, "Elhone request failed");
                Self::InternalServerError(message)
            }
            #[cfg(feature = "linux")]
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

fn parse_vm_id(value: String) -> Result<VmId, ApiError> {
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
                "vcpu_count": 1,
                "memory_mib": 128
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
            "vcpu_count": 1,
            "memory_mib": 128
        }))
        .expect_err("user must not be accepted");

        assert!(error.to_string().contains("unknown field `user`"));
    }

    #[test]
    fn vm_input_accepts_typed_port_bindings_and_defaults_to_none() {
        let without_bindings = serde_json::from_value::<VmInput>(json!({
            "vcpu_count": 1,
            "memory_mib": 128
        }))
        .expect("port bindings should be optional");
        assert!(without_bindings.port_bindings.is_empty());

        let with_bindings = serde_json::from_value::<VmInput>(json!({
            "vcpu_count": 1,
            "memory_mib": 128,
            "port_bindings": [
                {
                    "internal": 22,
                    "external": 2222
                }
            ]
        }))
        .expect("valid port bindings should deserialize");
        assert_eq!(
            with_bindings.port_bindings,
            vec![PortBinding {
                internal: Port::try_from(22).expect("valid internal port"),
                external: Port::try_from(2222).expect("valid external port"),
            }]
        );
    }

    #[test]
    fn vm_input_rejects_zero_port_bindings() {
        for field in ["internal", "external"] {
            let mut binding = json!({
                "internal": 22,
                "external": 2222
            });
            binding[field] = 0.into();

            let error = serde_json::from_value::<VmInput>(json!({
                "vcpu_count": 1,
                "memory_mib": 128,
                "port_bindings": [binding]
            }))
            .expect_err("port zero must be rejected");
            assert!(error.to_string().contains("invalid network port"));
        }
    }

    #[test]
    fn vm_summary_serializes_network() {
        let id = VmId::try_from(0).expect("valid VM ID");
        let summary = VmSummary {
            id,
            status: VmStatus::Running,
            network: NetworkSpec::new(id).expect("valid network"),
            port_bindings: vec![PortBinding {
                internal: Port::try_from(22).expect("valid internal port"),
                external: Port::try_from(2222).expect("valid external port"),
            }],
        };

        assert_eq!(
            serde_json::to_value(summary).expect("summary should serialize"),
            json!({
                "id": 0,
                "status": "running",
                "port_bindings": [
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
