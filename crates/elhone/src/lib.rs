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
        Err(ApiError::new(StatusCode::FORBIDDEN, "forbidden"))
    }
}

#[derive(Serialize)]
struct IdResponse {
    id: VmId,
}

#[derive(Serialize)]
struct StatusResponse {
    id: VmId,
    status: VmStatus,
}

async fn list_vms(State(state): State<AppState>) -> Json<Vec<barbirolli::VmSummary>> {
    Json(state.manager.list().await)
}

async fn create_vm(
    State(state): State<AppState>,
    input: Result<Json<VmInput>, JsonRejection>,
) -> Result<(StatusCode, Json<IdResponse>), ApiError> {
    let Json(input) = input
        .map_err(|error| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, error.body_text()))?;
    let id = state.manager.create(input).await.map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(IdResponse { id })))
}

async fn vm_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StatusResponse>, ApiError> {
    let id = parse_vm_id(id)?;
    status_json(&state.manager, id).await
}

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

async fn delete_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = parse_vm_id(id)?;
    state.manager.delete(id).await.map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn status_json(manager: &Barbirolli, id: VmId) -> Result<Json<StatusResponse>, ApiError> {
    let summary = manager.vm_mut(id).map_err(ApiError::from)?.summary();
    Ok(Json(StatusResponse {
        id,
        status: summary.status,
    }))
}

fn parse_vm_id(value: String) -> Result<VmId, ApiError> {
    let raw = value.parse::<u16>().map_err(|_| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("invalid VM ID {value:?}"),
        )
    })?;
    VmId::try_from(raw)
        .map_err(|error| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "route not found")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl From<LifecycleError> for ApiError {
    fn from(error: LifecycleError) -> Self {
        let status = match &error {
            LifecycleError::NotFound(_) => StatusCode::NOT_FOUND,
            LifecycleError::Storage(StorageError::DuplicateUser(_)) => StatusCode::CONFLICT,
            LifecycleError::Storage(StorageError::InvalidInput(_) | StorageError::IdsExhausted) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            LifecycleError::Draining | LifecycleError::InvalidTransition { .. } => {
                StatusCode::CONFLICT
            }
            LifecycleError::Storage(
                StorageError::SocketDirectory
                | StorageError::CreatingDirectory
                | StorageError::Io { .. }
                | StorageError::InvalidConfig { .. },
            )
            | LifecycleError::Shutdown(_)
            | LifecycleError::Warmup(_) => StatusCode::INTERNAL_SERVER_ERROR,
            #[cfg(not(feature = "linux"))]
            LifecycleError::UnsupportedPlatform => StatusCode::INTERNAL_SERVER_ERROR,
            #[cfg(feature = "linux")]
            LifecycleError::Vm(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(%error, "Elhone request failed");
        }
        Self::new(status, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            error: String,
        }
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}
