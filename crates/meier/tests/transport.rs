#![cfg(target_os = "linux")]

use std::str::FromStr;
use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode},
    response::Response,
    routing::any,
};
use meier::{
    cli::{
        Cli, Command, CreateArgs, IdArgs, ImageReference, OciCommand, PortMapping, VmCommand, VmId,
    },
    config::{ClientConfig, Config},
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, oneshot},
};

#[derive(Clone)]
struct Fixture {
    expected_method: String,
    expected_path: String,
    check_create: bool,
    response_body: String,
    result: Arc<Mutex<Option<oneshot::Sender<Result<RequestSnapshot, String>>>>>,
}

#[derive(Debug)]
struct RequestSnapshot {
    method: String,
    path: String,
    body: String,
}

#[tokio::test]
async fn maps_vm_and_oci_operations_to_elhone_requests() {
    let cases = [
        (
            "POST",
            "/vms",
            r#"{"id":0}"#,
            true,
            Command::Vm(VmCommand::Create(CreateArgs {
                vcpu_count: 2,
                publish: vec![PortMapping {
                    external: 2222,
                    internal: 22,
                }],
                authorized_key_file: Vec::new(),
            })),
        ),
        (
            "GET",
            "/vms/0/status",
            r#"{"status":"running"}"#,
            false,
            Command::Vm(VmCommand::Status(IdArgs {
                id: VmId::from_str("0").expect("id"),
            })),
        ),
        (
            "POST",
            "/vms/0/shutdown",
            "",
            false,
            Command::Vm(VmCommand::Shutdown(IdArgs {
                id: VmId::from_str("0").expect("id"),
            })),
        ),
        (
            "DELETE",
            "/vms/0",
            "",
            false,
            Command::Vm(VmCommand::Delete(IdArgs {
                id: VmId::from_str("0").expect("id"),
            })),
        ),
        (
            "POST",
            "/oci/rm",
            "",
            false,
            Command::Oci(OciCommand::Rm(meier::cli::ImageArgs {
                image: "alpine:latest".parse::<ImageReference>().expect("image"),
            })),
        ),
    ];

    for (method, path, body, check_create, command) in cases {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
        let address = listener.local_addr().expect("address");
        let (seen_tx, seen_rx) = oneshot::channel();
        let fixture = Fixture {
            expected_method: method.to_owned(),
            expected_path: path.to_owned(),
            check_create,
            response_body: body.to_owned(),
            result: Arc::new(Mutex::new(Some(seen_tx))),
        };
        let app = Router::new()
            .fallback(any(receive_request))
            .with_state(fixture.clone());
        let server = tokio::spawn(axum::serve(listener, app));

        let mut config = Config::default();
        config.client = ClientConfig {
            url: format!("http://{address}"),
        };
        let cli = Cli {
            config: None,
            command,
        };
        cli.dispatch_with_config(std::path::Path::new("unused.json"), &config)
            .await
            .expect("request should succeed");

        let snapshot = seen_rx
            .await
            .expect("request should be observed")
            .expect("fixture should accept the request");
        assert_eq!(snapshot.method, method);
        assert_eq!(snapshot.path, path);
        if check_create {
            assert!(snapshot.body.contains("\"vcpu_count\":2"));
            assert!(snapshot.body.contains("\"external\":2222"));
        }
        server.abort();
    }
}

async fn receive_request(State(fixture): State<Fixture>, request: Request<Body>) -> Response<Body> {
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    let body = match to_bytes(request.into_body(), usize::MAX).await {
        Ok(body) => String::from_utf8_lossy(&body).into_owned(),
        Err(error) => {
            let mut sender = fixture.result.lock().await;
            if let Some(sender) = sender.take() {
                sender
                    .send(Err(format!("request body could not be read: {error}")))
                    .expect("test should still be waiting");
            }
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .expect("response should build");
        }
    };
    let accepted = method == fixture.expected_method
        && path == fixture.expected_path
        && (!fixture.check_create
            || (body.contains("\"vcpu_count\":2") && body.contains("\"external\":2222")));
    let result = if accepted {
        Ok(RequestSnapshot { method, path, body })
    } else {
        Err(format!(
            "unexpected request: {method} {path} with body {body:?}"
        ))
    };
    let response_body = fixture.response_body;
    let response = if response_body.is_empty() {
        Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .expect("response should build")
    } else {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(response_body))
            .expect("response should build")
    };
    let mut sender = fixture.result.lock().await;
    if let Some(sender) = sender.take() {
        sender.send(result).expect("test should still be waiting");
    }
    response
}
