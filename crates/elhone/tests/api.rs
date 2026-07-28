#[cfg(not(feature = "linux"))]
use std::path::Path;
use std::{fs, sync::Arc};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use barbirolli::{ArtifactName, Barbirolli, MemoryMib, UserName, VcpuCount, VmInput, VmStore};
use elhone::{Auth, router};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

const AUTHORIZED_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBGdsGblMxDzs/KaTt+UP4TagL9vlgW2N9kHCOXVsmdQ test";

async fn fixture() -> (TempDir, Arc<Barbirolli>) {
    let temporary = TempDir::new().unwrap();
    let images = temporary.path().join("images");
    fs::create_dir(&images).unwrap();
    fs::write(images.join("vmlinux"), b"kernel").unwrap();
    fs::write(images.join("alpine.ext4"), b"rootfs").unwrap();
    let keys = temporary.path().join("authorized_keys");
    fs::write(&keys, format!("{AUTHORIZED_KEY}\n")).unwrap();
    let store = VmStore::new(temporary.path().join("vms"), images, keys).unwrap();
    #[cfg(feature = "linux")]
    let firecracker = {
        use std::os::unix::fs::PermissionsExt;

        let firecracker = temporary.path().join("firecracker");
        fs::write(&firecracker, "#!/bin/sh\necho 'Firecracker v1.13.0'\n").unwrap();
        fs::set_permissions(&firecracker, fs::Permissions::from_mode(0o755)).unwrap();
        firecracker
    };
    #[cfg(not(feature = "linux"))]
    let firecracker = Path::new("/usr/bin/firecracker").to_owned();
    let manager = Barbirolli::new(store, firecracker).await.unwrap();
    (temporary, Arc::new(manager))
}

fn vm_input(user: &str) -> VmInput {
    VmInput {
        user: user.parse::<UserName>().unwrap(),
        vcpu_count: VcpuCount::try_from(2).unwrap(),
        memory_mib: MemoryMib::try_from(1024).unwrap(),
        kernel: "vmlinux".parse::<ArtifactName>().unwrap(),
        rootfs: "alpine.ext4".parse::<ArtifactName>().unwrap(),
        authorized_keys: Vec::new(),
    }
}

async fn response_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn request(method: &str, uri: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer secret")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap()
}

#[cfg(not(feature = "local"))]
#[tokio::test]
async fn production_router_rejects_missing_or_wrong_credentials_with_json() {
    let (_temporary, manager) = fixture().await;
    let app = router(manager, Auth::bearer("secret"));
    for authorization in [None, Some("Bearer wrong")] {
        let mut builder = Request::builder().uri("/vms");
        if let Some(value) = authorization {
            builder = builder.header(header::AUTHORIZATION, value);
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response_json(response).await,
            serde_json::json!({"error": "forbidden"})
        );
    }
}

#[cfg(feature = "local")]
#[tokio::test]
async fn local_router_bypasses_http_authentication() {
    let (_temporary, manager) = fixture().await;
    let response = router(manager, Auth::bearer("ignored"))
        .oneshot(Request::builder().uri("/vms").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn api_exposes_create_list_status_shutdown_and_delete_contracts() {
    let (_temporary, manager) = fixture().await;
    let app = router(manager, Auth::bearer("secret"));
    let create = request(
        "POST",
        "/vms",
        Body::from(serde_json::to_vec(&vm_input("alice")).unwrap()),
    );
    let response = app.clone().oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response_json(response).await, serde_json::json!({"id": 0}));

    let response = app
        .clone()
        .oneshot(request("GET", "/vms", Body::empty()))
        .await
        .unwrap();
    assert_eq!(
        response_json(response).await,
        serde_json::json!([{"id": 0, "user": "alice", "status": "discovered"}])
    );

    let response = app
        .clone()
        .oneshot(request("POST", "/vms/0/shutdown", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        serde_json::json!({"id": 0, "status": "discovered"})
    );

    let response = app
        .clone()
        .oneshot(request("DELETE", "/vms/0", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "");
}

#[tokio::test]
async fn api_maps_unknown_invalid_and_duplicate_requests_to_json_errors() {
    let (_temporary, manager) = fixture().await;
    let app = router(manager, Auth::bearer("secret"));

    let response = app
        .clone()
        .oneshot(request("GET", "/vms/99999/status", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(response_json(response).await.get("error").is_some());

    let response = app
        .clone()
        .oneshot(request("GET", "/vms/4/status", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let body = serde_json::to_vec(&vm_input("alice")).unwrap();
    app.clone()
        .oneshot(request("POST", "/vms", Body::from(body.clone())))
        .await
        .unwrap();
    let response = app
        .clone()
        .oneshot(request("POST", "/vms", Body::from(body)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(response_json(response).await.get("error").is_some());

    let response = app
        .oneshot(request("PATCH", "/vms", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        response_json(response).await,
        serde_json::json!({"error": "method not allowed"})
    );
}
