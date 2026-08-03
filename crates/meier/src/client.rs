use std::{
    fs::File,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use futures::StreamExt;
use reqwest::Method;
use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::Command, time::sleep};

use crate::{
    MeierError,
    cli::SshArgs,
    config::{self, Config},
    error::process_exit_code,
    lima,
};

const STATUS_MARKER: &str = "\n__MEIER_HTTP_STATUS__";

#[allow(dead_code)]
enum Transport {
    Local(reqwest::Client),
    Lima { instance: String },
}

pub struct ApiClient {
    config: Config,
    transport: Transport,
}

impl ApiClient {
    pub async fn get(&self, path: &str) -> Result<Value, MeierError> {
        self.request(Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Option<Value>) -> Result<Value, MeierError> {
        self.request(Method::POST, path, body).await
    }

    pub async fn delete(&self, path: &str) -> Result<Value, MeierError> {
        self.request(Method::DELETE, path, None).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, MeierError> {
        tracing::info!(%method, path, "sending API request");
        match &self.transport {
            Transport::Local(client) => {
                let url = format!("{}{}", self.config.client.url.trim_end_matches('/'), path);
                let mut request = client
                    .request(method, url)
                    .header("Accept", "application/json");
                if let Some(body) = body {
                    request = request
                        .header("Content-Type", "application/json")
                        .json(&body);
                }
                let response = request.send().await?;
                let status = response.status();
                let bytes = response.bytes().await?;
                http_response(status.as_u16(), bytes).json()
            }
            Transport::Lima { instance } => {
                let mut args = vec![
                    "curl".to_owned(),
                    "--silent".to_owned(),
                    "--show-error".to_owned(),
                    "--fail-with-body".to_owned(),
                    "--request".to_owned(),
                    method.to_string(),
                    "--header".to_owned(),
                    "Accept: application/json".to_owned(),
                    "--write-out".to_owned(),
                    format!("{STATUS_MARKER}%{{http_code}}"),
                ];
                if let Some(body) = body {
                    args.extend([
                        "--header".to_owned(),
                        "Content-Type: application/json".to_owned(),
                        "--data".to_owned(),
                        serde_json::to_string(&body)?,
                    ]);
                }
                args.push(format!(
                    "{}{}",
                    self.config.client.url.trim_end_matches('/'),
                    path
                ));
                let output = lima::capture(instance, &args).await?;
                http_response_from_lima(output)?.json()
            }
        }
    }

    #[tracing::instrument(skip(self), fields(url = %url), err)]
    pub async fn external_json(&self, url: &str) -> Result<Value, MeierError> {
        match &self.transport {
            Transport::Local(client) => {
                let response = client.get(url).send().await?;
                let status = response.status();
                let bytes = response.bytes().await?;
                http_response(status.as_u16(), bytes).json()
            }
            Transport::Lima { instance } => {
                let output = lima::capture(
                    instance,
                    &[
                        "curl".to_owned(),
                        "--silent".to_owned(),
                        "--show-error".to_owned(),
                        "--fail-with-body".to_owned(),
                        url.to_owned(),
                    ],
                )
                .await?;
                if !output.status.success() {
                    return Err(MeierError::Http {
                        status: 502,
                        message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                    });
                }
                serde_json::from_slice(&output.stdout).map_err(MeierError::from)
            }
        }
    }

    #[tracing::instrument(skip(self), fields(vm_id = id, follow), err)]
    pub async fn stream_logs(&self, id: u16, follow: bool) -> Result<(), MeierError> {
        let query = if follow { "true" } else { "false" };
        match &self.transport {
            Transport::Lima { instance } => {
                lima::passthrough(
                    instance,
                    &[
                        "curl".to_owned(),
                        "--fail-with-body".to_owned(),
                        "--silent".to_owned(),
                        "--show-error".to_owned(),
                        "--no-buffer".to_owned(),
                        format!(
                            "{}/vms/{id}/logs?follow={query}",
                            self.config.client.url.trim_end_matches('/')
                        ),
                    ],
                )
                .await
            }
            Transport::Local(client) => {
                let url = format!(
                    "{}/vms/{id}/logs?follow={query}",
                    self.config.client.url.trim_end_matches('/')
                );
                let response = client.get(url).send().await?;
                if !response.status().is_success() {
                    let status = response.status().as_u16();
                    let body = response.bytes().await?;
                    return Err(http_response(status, body).error());
                }
                let mut stream = response.bytes_stream();
                let mut stdout = tokio::io::stdout();
                while let Some(chunk) = stream.next().await {
                    stdout.write_all(&chunk?).await?;
                    stdout.flush().await?;
                }
                Ok(())
            }
        }
    }
}

pub(crate) async fn connect(config: &Config) -> Result<ApiClient, MeierError> {
    // `reqwest` is built with `rustls-no-provider` so the binary controls
    // provider installation explicitly and does not panic on first use.
    let _ = rustls::crypto::ring::default_provider().install_default();
    #[cfg(target_os = "macos")]
    {
        lima::ensure_running(&config.lima.instance).await?;
        Ok(ApiClient {
            config: config.clone(),
            transport: Transport::Lima {
                instance: config.lima.instance.clone(),
            },
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(ApiClient {
            config: config.clone(),
            transport: Transport::Local(client),
        })
    }
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn http_response(status: u16, body: impl Into<Vec<u8>>) -> HttpResponse {
    HttpResponse {
        status,
        body: body.into(),
    }
}

fn http_response_from_lima(output: std::process::Output) -> Result<HttpResponse, MeierError> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some((body, status)) = stdout.rsplit_once(STATUS_MARKER) else {
        return Err(MeierError::Lima {
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    };
    let status = status
        .trim()
        .parse::<u16>()
        .map_err(|error| MeierError::Lima {
            message: format!("invalid HTTP status from guest curl: {error}"),
        })?;
    Ok(http_response(status, body.as_bytes()))
}

impl HttpResponse {
    fn json(&self) -> Result<Value, MeierError> {
        if self.status == 204 {
            return Ok(Value::Null);
        }
        if !(200..300).contains(&self.status) {
            return Err(self.error());
        }
        if self.body.is_empty() || self.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&self.body).map_err(MeierError::from)
    }

    fn error(&self) -> MeierError {
        let message = serde_json::from_slice::<Value>(&self.body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| String::from_utf8_lossy(&self.body).trim().to_owned());
        MeierError::Http {
            status: self.status,
            message,
        }
    }
}

impl ApiClient {
    #[tracing::instrument(skip(self, args), fields(vm_id = %args.id), err)]
    pub async fn ssh(&self, args: &SshArgs) -> Result<(), MeierError> {
        let summary = self.get(&format!("/vms/{}", args.id)).await?;
        let status = summary
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !status.eq_ignore_ascii_case("running") {
            return Err(MeierError::Validation(format!(
                "VM {} is not running (status: {status}); run `meier vm start {}`",
                args.id, args.id
            )));
        }
        let guest_ip = summary
            .pointer("/network/guest_ip")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                MeierError::Validation("VM summary has no network.guest_ip".to_owned())
            })?;
        let identity = args.identity.clone().map(Ok).unwrap_or_else(|| {
            config::runtime_paths(&self.config).map(|paths| paths.private_key)
        })?;
        if !identity.is_file() || File::open(&identity).is_err() {
            return Err(MeierError::Validation(format!(
                "SSH identity file is missing or unreadable: {}",
                identity.display()
            )));
        }
        let identity = guest_identity(&self.config, identity).await?;
        wait_for_ssh(&self.config, &identity, guest_ip).await?;
        let mut command =
            ssh_command(&self.config, &identity, guest_ip, &args.command, false).await?;
        tracing::info!(vm_id = %args.id, %guest_ip, "starting SSH session");
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = command.status().await?;
        if status.success() {
            Ok(())
        } else {
            Err(MeierError::ProcessExit {
                program: "ssh".to_owned(),
                code: process_exit_code(status),
            })
        }
    }
}

async fn wait_for_ssh(config: &Config, identity: &Path, guest_ip: &str) -> Result<(), MeierError> {
    for _ in 0..30 {
        let mut command =
            ssh_command(config, identity, guest_ip, &["true".to_owned()], true).await?;
        command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        if command.status().await?.success() {
            return Ok(());
        }
        sleep(Duration::from_secs(1)).await;
    }
    Err(MeierError::Validation(format!(
        "SSH did not become ready for {guest_ip} within 30 seconds"
    )))
}

async fn ssh_command(
    config: &Config,
    identity: &Path,
    guest_ip: &str,
    remote_command: &[String],
    batch_mode: bool,
) -> Result<Command, MeierError> {
    let mut args = vec![
        "-i".to_owned(),
        identity.display().to_string(),
        "-o".to_owned(),
        "IdentitiesOnly=yes".to_owned(),
        "-o".to_owned(),
        "StrictHostKeyChecking=no".to_owned(),
        "-o".to_owned(),
        "UserKnownHostsFile=/dev/null".to_owned(),
        "-o".to_owned(),
        "ConnectTimeout=1".to_owned(),
        "-o".to_owned(),
        "ConnectionAttempts=1".to_owned(),
    ];
    if batch_mode {
        args.extend(["-o".to_owned(), "BatchMode=yes".to_owned()]);
    }
    args.push(format!("root@{guest_ip}"));
    args.extend(remote_command.iter().cloned());
    #[cfg(target_os = "macos")]
    {
        let mut command =
            Command::new(std::env::var("LIMACTL").unwrap_or_else(|_| "limactl".to_owned()));
        command
            .arg("shell")
            .arg(&config.lima.instance)
            .arg("--")
            .arg("ssh")
            .args(args);
        Ok(command)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Command::new("ssh").args(args))
    }
}

async fn guest_identity(config: &Config, identity: PathBuf) -> Result<PathBuf, MeierError> {
    #[cfg(target_os = "macos")]
    {
        let default_identity = config::runtime_paths(config)?.private_key;
        if identity == default_identity {
            let home = lima::guest_home(&config.lima.instance).await?;
            let runtime = match config.daemon.runtime_dir.as_str() {
                "~" => PathBuf::from(home),
                value if value.starts_with("~/") => {
                    PathBuf::from(home).join(value.trim_start_matches("~/"))
                }
                value => PathBuf::from(value),
            };
            return Ok(runtime.join("ssh/id_ed25519"));
        }
    }
    let _ = config;
    Ok(identity)
}
