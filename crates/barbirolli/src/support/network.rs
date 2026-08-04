use std::{
    net::{Ipv4Addr, TcpListener as StdTcpListener},
    sync::Arc,
    time::Duration,
};

use barbirolli::NetworkSpec;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

#[derive(Clone)]
pub struct VmNetworkFixture {
    pub spec: NetworkSpec,
}

#[derive(Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub struct LimaHttpFixture {
    port: u16,
    task: JoinHandle<()>,
}

impl VmNetworkFixture {
    #[must_use]
    pub fn new(spec: NetworkSpec) -> Self {
        Self { spec }
    }

    #[must_use]
    pub fn url(&self, port: u16) -> String {
        format!("http://{}:{port}", self.spec.guest_ip)
    }

    pub async fn http_get(&self, port: u16, inspect: impl FnOnce(&HttpResponse)) -> HttpResponse {
        let mut last_error = None;
        for _ in 0..50 {
            match request_http(self.spec.guest_ip, port).await {
                Ok(response) => {
                    inspect(&response);
                    return response;
                }
                Err(error) => {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        panic!(
            "guest HTTP server did not become reachable: {}",
            last_error.expect("an HTTP attempt should have failed")
        );
    }

    pub async fn try_http_get(&self, port: u16) -> Result<HttpResponse, String> {
        request_http(self.spec.guest_ip, port)
            .await
            .map_err(|error| error.to_string())
    }
}

impl LimaHttpFixture {
    pub async fn start(body: impl Into<Vec<u8>>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .expect("failed to bind the Lima HTTP fixture");
        let port = listener
            .local_addr()
            .expect("failed to read the Lima HTTP fixture address")
            .port();
        let body = Arc::new(body.into());
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut connection, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = [0; 1024];
                    if let Err(error) = connection.read(&mut request).await {
                        tracing::debug!(%error, "HTTP fixture could not read the request");
                        return;
                    }
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    if let Err(error) = connection.write_all(headers.as_bytes()).await {
                        tracing::debug!(%error, "HTTP fixture could not write headers");
                        return;
                    }
                    if let Err(error) = connection.write_all(&body).await {
                        tracing::debug!(%error, "HTTP fixture could not write the body");
                    }
                });
            }
        });
        Self { port, task }
    }

    #[must_use]
    pub fn url_for(&self, vm: &VmNetworkFixture) -> String {
        format!("http://{}:{}", vm.spec.host_ip, self.port)
    }
}

impl Drop for LimaHttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[must_use]
pub fn available_tcp_port() -> u16 {
    StdTcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
        .expect("failed to reserve an available TCP port")
        .local_addr()
        .expect("failed to read the available TCP port")
        .port()
}

async fn request_http(address: Ipv4Addr, port: u16) -> Result<HttpResponse, reqwest::Error> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(2))
        .build()?;
    let response = client
        .get(format!("http://{address}:{port}"))
        .send()
        .await?;
    let status = response.status().as_u16();
    let body = response.bytes().await?.to_vec();
    Ok(HttpResponse { status, body })
}
