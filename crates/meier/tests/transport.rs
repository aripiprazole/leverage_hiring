#![cfg(target_os = "linux")]

use std::str::FromStr;

use meier::{
    cli::{
        Cli, Command, CreateArgs, IdArgs, ImageReference, OciCommand, PortMapping, VmCommand, VmId,
    },
    config::{ClientConfig, Config},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};

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
        let body = body.to_owned();
        let method = method.to_owned();
        let path = path.to_owned();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let request = read_request(&mut socket).await;
            assert_eq!(request.0, method);
            assert_eq!(request.1, path);
            if check_create {
                assert!(request.2.contains("\"vcpu_count\":2"));
                assert!(request.2.contains("\"external\":2222"));
            }
            let _ = seen_tx.send(());
            let response = if body.is_empty() {
                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_owned()
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
            };
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response");
        });

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
        seen_rx.await.expect("request should be observed");
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, String, String) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await.expect("request bytes");
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = header
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
            })
            .unwrap_or(0);
        if bytes.len() >= header_end + 4 + content_length {
            break;
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().expect("request line");
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().expect("method").to_owned();
    let path = request_parts.next().expect("path").to_owned();
    let body = text
        .split_once("\r\n\r\n")
        .map_or("", |(_, body)| body)
        .to_owned();
    (method, path, body)
}
