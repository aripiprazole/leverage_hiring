//! Cross-platform command-line access to the Barbirolli VM service.

pub mod cli {
    use std::{
        fs,
        path::{Path, PathBuf},
        str::FromStr,
    };

    use clap::{Args, Parser, Subcommand, error::ErrorKind};
    use serde_json::{Value, json};

    use crate::{
        MeierError, Result,
        client::ApiClient,
        config::{self, Config},
        setup,
    };

    #[derive(Debug, Parser)]
    #[command(
        name = "meier",
        version,
        about = "Manage Barbirolli Firecracker microVMs"
    )]
    pub struct Cli {
        /// Configuration file. Defaults to ~/.meier/config.json.
        #[arg(long, global = true, value_name = "PATH")]
        pub config: Option<PathBuf>,
        #[command(subcommand)]
        pub command: Command,
    }

    #[derive(Debug, Subcommand)]
    pub enum Command {
        #[command(subcommand)]
        Config(ConfigCommand),
        #[command(subcommand)]
        Daemon(DaemonCommand),
        #[command(subcommand)]
        Vm(VmCommand),
        #[command(subcommand)]
        Oci(OciCommand),
        /// Private foreground daemon mode used by the feature-enabled binary.
        #[cfg(feature = "daemon")]
        #[command(name = "__daemon", hide = true)]
        InternalDaemon(DaemonInternalArgs),
    }

    #[derive(Debug, Subcommand)]
    pub enum ConfigCommand {
        /// Create ~/.meier/config.json with safe defaults.
        Init,
    }

    #[derive(Debug, Subcommand)]
    pub enum DaemonCommand {
        /// Prepare Firecracker, the guest kernel, rootfs, and SSH key.
        Setup,
        /// Run the private Linux daemon in the foreground.
        Run,
    }

    /// Arguments for the hidden daemon mode of the `meier` executable.
    ///
    /// Keeping this command in the same parser as the public CLI means setup can
    /// install one executable while the `daemon` feature controls whether that
    /// executable contains the Linux service runner.
    #[cfg(feature = "daemon")]
    #[derive(Debug, Args)]
    pub struct DaemonInternalArgs {
        /// Absolute runtime directory supplied by the launcher after privilege
        /// escalation, avoiding a changed HOME under sudo.
        #[arg(long, value_name = "PATH")]
        pub runtime_dir: Option<PathBuf>,
    }

    #[derive(Debug, Subcommand)]
    pub enum VmCommand {
        Create(CreateArgs),
        Ps,
        Show(IdArgs),
        Status(IdArgs),
        Start(IdArgs),
        Shutdown(IdArgs),
        Delete(IdArgs),
        Ssh(SshArgs),
        Logs(LogsArgs),
    }

    #[derive(Debug, Args)]
    pub struct CreateArgs {
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=31))]
        pub vcpu_count: u8,
        #[arg(long = "publish", value_name = "EXTERNAL:INTERNAL")]
        pub publish: Vec<PortMapping>,
        #[arg(long = "authorized-key-file", value_name = "PATH")]
        pub authorized_key_file: Vec<PathBuf>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PortMapping {
        pub external: u16,
        pub internal: u16,
    }

    impl FromStr for PortMapping {
        type Err = String;

        fn from_str(value: &str) -> Result<Self, Self::Err> {
            let (external, internal) = value
                .split_once(':')
                .filter(|(external, internal)| {
                    !external.is_empty() && !internal.is_empty() && !internal.contains(':')
                })
                .ok_or_else(|| format!("expected EXTERNAL:INTERNAL, got {value:?}"))?;
            let external = external
                .parse::<u16>()
                .map_err(|_| format!("invalid external port in {value:?}; expected 1-65535"))?;
            let internal = internal
                .parse::<u16>()
                .map_err(|_| format!("invalid internal port in {value:?}; expected 1-65535"))?;
            if external == 0 || internal == 0 {
                return Err(format!(
                    "invalid publish mapping {value:?}; ports must be 1-65535"
                ));
            }
            Ok(Self { external, internal })
        }
    }

    #[derive(Debug, Args)]
    pub struct IdArgs {
        pub id: VmId,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub struct VmId(u16);

    impl VmId {
        #[must_use]
        pub const fn get(&self) -> u16 {
            self.0
        }
    }

    impl FromStr for VmId {
        type Err = String;

        fn from_str(value: &str) -> Result<Self, Self::Err> {
            let id = value
                .parse::<u16>()
                .map_err(|_| "VM ID must be an integer from 0 to 16383".to_owned())?;
            (id <= 16_383)
                .then_some(Self(id))
                .ok_or_else(|| "VM ID must be an integer from 0 to 16383".to_owned())
        }
    }

    impl std::fmt::Display for VmId {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(formatter)
        }
    }

    #[derive(Debug, Args)]
    pub struct SshArgs {
        #[arg(long, value_name = "PATH")]
        pub identity: Option<PathBuf>,
        pub id: VmId,
        #[arg(trailing_var_arg = true)]
        pub command: Vec<String>,
    }

    #[derive(Debug, Args)]
    pub struct LogsArgs {
        pub id: VmId,
        #[arg(short = 'a', long, conflicts_with = "pull")]
        pub attach: bool,
        #[arg(short = 'p', long, conflicts_with = "attach")]
        pub pull: bool,
    }

    #[derive(Debug, Subcommand)]
    pub enum OciCommand {
        Pull(ImageArgs),
        Run(ImageArgs),
        Stop(ImageArgs),
        Rm(ImageArgs),
    }

    #[derive(Debug, Args)]
    pub struct ImageArgs {
        pub image: ImageReference,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ImageReference {
        pub name: String,
        pub tag: String,
    }

    impl FromStr for ImageReference {
        type Err = String;

        fn from_str(value: &str) -> Result<Self, Self::Err> {
            let (name, tag) = value.rsplit_once(':').ok_or_else(|| {
                format!("expected a Docker image like alpine:latest, got {value:?}")
            })?;
            if name.is_empty() || tag.is_empty() {
                return Err(format!("invalid Docker image {value:?}"));
            }
            if !valid_image_name(name) {
                return Err(format!("invalid Docker Hub image name {name:?}"));
            }
            if !valid_image_tag(tag) {
                return Err(format!("invalid Docker Hub image tag {tag:?}"));
            }
            Ok(Self {
                name: name.to_owned(),
                tag: tag.to_owned(),
            })
        }
    }

    fn valid_image_name(name: &str) -> bool {
        name.split('/').all(|component| {
            !component.is_empty()
                && component.split(['.', '_', '-']).all(|part| {
                    !part.is_empty()
                        && part
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                })
        })
    }

    fn valid_image_tag(tag: &str) -> bool {
        let mut chars = tag.chars();
        matches!(chars.next(), Some(first) if first.is_ascii_alphanumeric() || first == '_')
            && tag.len() <= 128
            && chars.all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
            })
    }

    /// Parse the process arguments, printing help or version output directly.
    ///
    /// # Errors
    ///
    /// Returns an error when the arguments do not form a valid command.
    pub fn parse_args() -> Result<Cli> {
        match Cli::try_parse() {
            Ok(cli) => Ok(cli),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                ) =>
            {
                print!("{error}");
                std::process::exit(0);
            }
            Err(error) => Err(MeierError::from(error)),
        }
    }

    impl Cli {
        #[must_use]
        pub fn is_bootstrap_command(&self) -> bool {
            matches!(self.command, Command::Config(ConfigCommand::Init))
        }

        #[must_use]
        pub(crate) fn has_json_output(&self) -> bool {
            match &self.command {
                Command::Daemon(DaemonCommand::Run)
                | Command::Vm(VmCommand::Ssh(_) | VmCommand::Logs(_)) => false,
                #[cfg(feature = "daemon")]
                Command::InternalDaemon(_) => false,
                _ => true,
            }
        }

        #[cfg(feature = "daemon")]
        #[must_use]
        pub fn internal_daemon_args(&self) -> Option<&DaemonInternalArgs> {
            match &self.command {
                Command::InternalDaemon(args) => Some(args),
                _ => None,
            }
        }
    }

    impl Cli {
        /// Dispatch a command that does not require an existing configuration.
        ///
        /// # Errors
        ///
        /// Returns an error when configuration initialization fails or when the
        /// selected command requires an existing configuration.
        pub fn dispatch(&self, config_path: &Path) -> Result<Value> {
            match &self.command {
                Command::Config(ConfigCommand::Init) => {
                    let config = config::init(config_path)?;
                    serde_json::to_value(config).map_err(MeierError::from)
                }
                _ => Err(MeierError::ConfigRead {
                    path: config_path.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "run `meier config init` before using the client",
                    ),
                }),
            }
        }

        /// Dispatch a command using an existing configuration.
        ///
        /// # Errors
        ///
        /// Returns an error when the command is invalid or its setup, transport,
        /// or daemon operation fails.
        pub async fn dispatch_with_config(
            &self,
            config_path: &Path,
            config: &Config,
        ) -> Result<Value> {
            // Do not record the parsed command: it can contain authorized-key
            // paths and, for `vm ssh`, arbitrary remote command arguments.
            tracing::info!("dispatching Meier command");
            match &self.command {
                Command::Config(ConfigCommand::Init) => Err(MeierError::Validation(
                    "configuration already exists; remove it explicitly before reinitializing"
                        .to_owned(),
                )),
                Command::Daemon(DaemonCommand::Setup) => {
                    setup::setup(config).await?;
                    Ok(json!({"status": "ready"}))
                }
                Command::Daemon(DaemonCommand::Run) => {
                    setup::run_daemon(config, config_path).await?;
                    Ok(Value::Null)
                }
                Command::Vm(command) => command.dispatch(config).await,
                Command::Oci(command) => command.dispatch(config).await,
                #[cfg(feature = "daemon")]
                Command::InternalDaemon(_) => Err(MeierError::Validation(
                    "the internal daemon command must be dispatched by the feature-enabled runner"
                        .to_owned(),
                )),
            }
        }
    }

    impl VmCommand {
        async fn dispatch(&self, config: &Config) -> Result<Value> {
            match self {
                Self::Create(args) => {
                    let body = args.payload()?;
                    config.connect().await?.post("/vms", Some(body)).await
                }
                Self::Ps => config.connect().await?.get("/vms").await,
                Self::Show(args) => args.show_status(config, "").await,
                Self::Status(args) => args.show_status(config, "/status").await,
                Self::Start(args) => args.mutate(config, "start").await,
                Self::Shutdown(args) => args.mutate(config, "shutdown").await,
                Self::Delete(args) => {
                    config
                        .connect()
                        .await?
                        .delete(&format!("/vms/{}", args.id))
                        .await
                }
                Self::Logs(args) => {
                    let follow = args.attach || !args.pull;
                    config
                        .connect()
                        .await?
                        .stream_logs(args.id.get(), follow)
                        .await?;
                    Ok(Value::Null)
                }
                Self::Ssh(args) => {
                    config.connect().await?.ssh(args).await?;
                    Ok(Value::Null)
                }
            }
        }
    }

    impl IdArgs {
        async fn show_status(&self, config: &Config, suffix: &str) -> Result<Value> {
            config
                .connect()
                .await?
                .get(&format!("/vms/{}{suffix}", self.id))
                .await
        }

        async fn mutate(&self, config: &Config, operation: &str) -> Result<Value> {
            config
                .connect()
                .await?
                .post(&format!("/vms/{}/{}", self.id, operation), None)
                .await
        }
    }

    impl OciCommand {
        async fn dispatch(&self, config: &Config) -> Result<Value> {
            let client = config.connect().await?;
            let value = match self {
                Self::Pull(args) => {
                    client
                        .post(
                            "/oci/pull",
                            Some(json!({
                                "name": &args.image.name,
                                "tag": &args.image.tag,
                                "token": args.image.docker_token(&client).await?,
                            })),
                        )
                        .await?
                }
                Self::Run(args) => {
                    client
                        .post(
                            "/oci/run",
                            Some(json!({
                                "name": &args.image.name,
                                "tag": &args.image.tag,
                                "token": args.image.docker_token(&client).await?,
                            })),
                        )
                        .await?
                }
                Self::Stop(args) => {
                    client
                        .post(
                            "/oci/stop",
                            Some(json!({
                                "name": &args.image.name,
                                "tag": &args.image.tag,
                            })),
                        )
                        .await?
                }
                Self::Rm(args) => {
                    client
                        .post(
                            "/oci/rm",
                            Some(json!({
                                "name": &args.image.name,
                                "tag": &args.image.tag,
                            })),
                        )
                        .await?
                }
            };
            Ok(value)
        }
    }

    impl ImageReference {
        fn repository(&self) -> String {
            if self.name.contains('/') {
                self.name.clone()
            } else {
                format!("library/{}", self.name)
            }
        }

        async fn docker_token(&self, client: &ApiClient) -> Result<String> {
            let token_response = client
            .external_json(&format!(
                "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
                self.repository()
            ))
            .await?;
            token_response
                .get("token")
                .or_else(|| token_response.get("access_token"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    MeierError::Validation("Docker Hub token response had no token".to_owned())
                })
        }
    }

    impl CreateArgs {
        #[tracing::instrument(skip(self), fields(authorized_key_file_count = self.authorized_key_file.len()))]
        pub(crate) fn payload(&self) -> Result<Value> {
            let bindings = self
                .publish
                .iter()
                .map(|mapping| {
                    json!({
                        "internal": mapping.internal,
                        "external": mapping.external,
                    })
                })
                .collect::<Vec<_>>();
            let mut authorized_keys = Vec::new();
            for path in &self.authorized_key_file {
                let contents = fs::read_to_string(path).map_err(|source| {
                    MeierError::Validation(format!(
                        "authorized key file {} could not be read: {source}",
                        path.display()
                    ))
                })?;
                let mut lines = contents.lines();
                let key = lines
                    .next()
                    .filter(|line| !line.is_empty() && !line.contains('\r'))
                    .ok_or_else(|| {
                        MeierError::Validation(format!(
                            "authorized key file {} must contain exactly one key",
                            path.display()
                        ))
                    })?;
                if lines.next().is_some() {
                    return Err(MeierError::Validation(format!(
                        "authorized key file {} must contain exactly one key",
                        path.display()
                    )));
                }
                authorized_keys.push(key.to_owned());
            }
            Ok(json!({
                "vcpu_count": self.vcpu_count,
                "authorized_keys": authorized_keys,
                "bindings": bindings,
            }))
        }
    }
}

mod client {
    use std::{path::Path, process::Stdio, time::Duration};

    #[cfg(not(target_os = "macos"))]
    use std::fs::File;
    #[cfg(target_os = "macos")]
    use std::path::PathBuf;

    use futures::StreamExt;
    use reqwest::Method;
    use serde_json::Value;
    use tokio::{io::AsyncWriteExt, process::Command, time::sleep};

    use crate::{MeierError, Result, cli::SshArgs, config::Config, error::process_exit_code, lima};

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
        pub async fn get(&self, path: &str) -> Result<Value> {
            self.request(Method::GET, path, None).await
        }

        pub async fn post(&self, path: &str, body: Option<Value>) -> Result<Value> {
            self.request(Method::POST, path, body).await
        }

        pub async fn delete(&self, path: &str) -> Result<Value> {
            self.request(Method::DELETE, path, None).await
        }

        async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
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
                    http_response_from_lima(&output)?.json()
                }
            }
        }

        #[tracing::instrument(skip(self), fields(url = %url), err)]
        pub async fn external_json(&self, url: &str) -> Result<Value> {
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
        pub async fn stream_logs(&self, id: u16, follow: bool) -> Result<()> {
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

    impl Config {
        #[cfg(target_os = "macos")]
        pub(crate) async fn connect(&self) -> Result<ApiClient> {
            lima::ensure_running(&self.lima.instance).await?;
            Ok(ApiClient {
                config: self.clone(),
                transport: Transport::Lima {
                    instance: self.lima.instance.clone(),
                },
            })
        }

        #[cfg(not(target_os = "macos"))]
        pub(crate) fn connect(&self) -> std::future::Ready<Result<ApiClient>> {
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .build()
                .map(|client| ApiClient {
                    config: self.clone(),
                    transport: Transport::Local(client),
                })
                .map_err(MeierError::from);
            std::future::ready(client)
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

    fn http_response_from_lima(output: &std::process::Output) -> Result<HttpResponse> {
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
        fn json(&self) -> Result<Value> {
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
            let message = match serde_json::from_slice::<Value>(&self.body) {
                Ok(value) => value.get("error").and_then(Value::as_str).map_or_else(
                    || String::from_utf8_lossy(&self.body).trim().to_owned(),
                    str::to_owned,
                ),
                Err(error) => {
                    tracing::debug!(%error, status = self.status, "HTTP error body was not JSON");
                    String::from_utf8_lossy(&self.body).trim().to_owned()
                }
            };
            MeierError::Http {
                status: self.status,
                message,
            }
        }
    }

    impl ApiClient {
        #[tracing::instrument(skip(self, args), fields(vm_id = %args.id), err)]
        pub async fn ssh(&self, args: &SshArgs) -> Result<()> {
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
            let default_identity = self.config.runtime_paths()?.private_key;
            let identity = args.identity.clone().unwrap_or(default_identity);
            #[cfg(target_os = "macos")]
            let identity = guest_identity(&self.config, identity).await?;
            #[cfg(not(target_os = "macos"))]
            if !identity.is_file() || File::open(&identity).is_err() {
                return Err(MeierError::Validation(format!(
                    "SSH identity file is missing or unreadable: {}",
                    identity.display()
                )));
            }
            wait_for_ssh(&self.config, &identity, guest_ip).await?;
            let mut command = ssh_command(&self.config, &identity, guest_ip, &args.command, false);
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

    async fn wait_for_ssh(config: &Config, identity: &Path, guest_ip: &str) -> Result<()> {
        for _ in 0..30 {
            let mut command = ssh_command(config, identity, guest_ip, &["true".to_owned()], true);
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

    fn ssh_command(
        config: &Config,
        identity: &Path,
        guest_ip: &str,
        remote_command: &[String],
        batch_mode: bool,
    ) -> Command {
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
            command
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = config;
            let mut command = Command::new("ssh");
            command.args(args);
            command
        }
    }

    #[cfg(target_os = "macos")]
    async fn guest_identity(config: &Config, identity: PathBuf) -> Result<PathBuf> {
        let default_identity = config.runtime_paths()?.private_key;
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
        Ok(identity)
    }
}

pub mod config {
    use std::{
        env, fs,
        net::SocketAddr,
        os::unix::fs::OpenOptionsExt,
        path::{Path, PathBuf},
    };

    use serde::{Deserialize, Serialize};

    use crate::{MeierError, Result};

    const DEFAULT_CONFIG_DIR: &str = ".meier";
    const DEFAULT_CONFIG_FILE: &str = "config.json";
    const DEFAULT_RUNTIME_DIR: &str = "~/.local/share/barbirolli";

    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Config {
        #[serde(default)]
        pub lima: LimaConfig,
        #[serde(default)]
        pub client: ClientConfig,
        #[serde(default)]
        pub daemon: DaemonConfig,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(default, deny_unknown_fields)]
    pub struct LimaConfig {
        pub instance: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(default, deny_unknown_fields)]
    pub struct ClientConfig {
        pub url: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(default, deny_unknown_fields)]
    pub struct DaemonConfig {
        pub listen: SocketAddr,
        pub runtime_dir: String,
    }

    impl Default for LimaConfig {
        fn default() -> Self {
            Self {
                instance: "provisioning".to_owned(),
            }
        }
    }

    impl Default for ClientConfig {
        fn default() -> Self {
            Self {
                url: "http://127.0.0.1:3000".to_owned(),
            }
        }
    }

    impl Default for DaemonConfig {
        fn default() -> Self {
            Self {
                listen: SocketAddr::from(([127, 0, 0, 1], 3000)),
                runtime_dir: DEFAULT_RUNTIME_DIR.to_owned(),
            }
        }
    }

    #[must_use]
    pub fn default_path() -> PathBuf {
        home_dir().map_or_else(
            || PathBuf::from(DEFAULT_CONFIG_DIR).join(DEFAULT_CONFIG_FILE),
            |home| home.join(DEFAULT_CONFIG_DIR).join(DEFAULT_CONFIG_FILE),
        )
    }

    #[must_use]
    pub fn home_dir() -> Option<PathBuf> {
        env::var_os("HOME").map(PathBuf::from)
    }

    /// Load and decode a configuration file.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or does not contain a
    /// valid Meier configuration.
    #[tracing::instrument(skip_all, fields(path = %path.display()), err)]
    pub fn load(path: &Path) -> Result<Config> {
        let bytes = fs::read(path).map_err(|source| MeierError::ConfigRead {
            path: path.to_owned(),
            source,
        })?;
        serde_json::from_slice(&bytes).map_err(|source| MeierError::ConfigDecode {
            path: path.to_owned(),
            source,
        })
    }

    /// Create a new configuration file with the documented defaults.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent directory or file cannot be created,
    /// the defaults cannot be serialized, or the file already exists.
    #[tracing::instrument(skip_all, fields(path = %path.display()), err)]
    pub fn init(path: &Path) -> Result<Config> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| MeierError::ConfigWrite {
            path: parent.to_owned(),
            source,
        })?;
        let config = Config::default();
        let encoded = serde_json::to_vec_pretty(&config)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|source| MeierError::ConfigWrite {
                path: path.to_owned(),
                source,
            })?;
        use std::io::Write;
        file.write_all(&encoded)
            .map_err(|source| MeierError::ConfigWrite {
                path: path.to_owned(),
                source,
            })?;
        file.write_all(b"\n")
            .map_err(|source| MeierError::ConfigWrite {
                path: path.to_owned(),
                source,
            })?;
        tracing::info!(path = %path.display(), "initialized Meier configuration");
        Ok(config)
    }

    impl DaemonConfig {
        #[tracing::instrument(skip(self), err)]
        fn expand_runtime_dir(&self) -> Result<PathBuf> {
            if self.runtime_dir == "~" {
                return home_dir()
                    .ok_or_else(|| MeierError::Validation("HOME is not set".to_owned()));
            }
            if let Some(rest) = self.runtime_dir.strip_prefix("~/") {
                return Ok(home_dir()
                    .ok_or_else(|| MeierError::Validation("HOME is not set".to_owned()))?
                    .join(rest));
            }
            Ok(PathBuf::from(&self.runtime_dir))
        }
    }

    #[derive(Debug, Clone)]
    pub struct RuntimePaths {
        pub root: PathBuf,
        pub downloads: PathBuf,
        pub images: PathBuf,
        pub vms: PathBuf,
        pub ssh: PathBuf,
        pub firecracker: PathBuf,
        pub entrypoint: PathBuf,
        pub kernel: PathBuf,
        pub rootfs: PathBuf,
        pub authorized_keys: PathBuf,
        pub private_key: PathBuf,
    }

    impl Config {
        /// Resolve the configured runtime directory into all managed paths.
        ///
        /// # Errors
        ///
        /// Returns an error when a home-relative runtime directory is configured
        /// but `HOME` is not set.
        #[tracing::instrument(skip(self), err)]
        pub fn runtime_paths(&self) -> Result<RuntimePaths> {
            let root = self.daemon.expand_runtime_dir()?;
            let downloads = root.join("downloads");
            let images = root.join("images");
            let vms = root.join("vms");
            let ssh = root.join("ssh");
            Ok(RuntimePaths {
                firecracker: root.join("bin/firecracker"),
                entrypoint: root.join("bin/barbirolli_entrypoint"),
                kernel: images.join("vmlinux"),
                rootfs: images.join("ubuntu-24.04.ext4"),
                authorized_keys: ssh.join("id_ed25519.pub"),
                private_key: ssh.join("id_ed25519"),
                root,
                downloads,
                images,
                vms,
                ssh,
            })
        }
    }
}

mod error {
    use std::{io, path::PathBuf};

    use thiserror::Error;

    #[derive(Debug, Error)]
    pub enum SetupError {
        #[error("could not execute {program}: {source}")]
        Command {
            program: String,
            #[source]
            source: io::Error,
        },
        #[error("missing required command `{command}` on the execution target")]
        MissingCommand { command: String },
        #[error("runtime artifacts are missing or invalid; run `meier daemon setup` first")]
        RuntimeArtifactsMissing,
        #[error("setup finished without producing all required runtime artifacts")]
        ArtifactsIncomplete,
        #[error("/dev/kvm is unavailable on the execution target")]
        KvmUnavailable,
        #[error("incomplete SSH key pair; move it aside and rerun setup")]
        IncompleteSshKey,
        #[error("unsupported target architecture `{value}`")]
        UnsupportedArchitecture { value: String },
        #[error("could not read workspace manifest {path}: {source}")]
        Workspace {
            path: PathBuf,
            #[source]
            source: io::Error,
        },
        #[error("daemon operation failed: {source}")]
        Daemon {
            #[source]
            source: Box<dyn std::error::Error + Send + Sync>,
        },
        #[error("daemon service failed: {source}")]
        DaemonService {
            #[source]
            source: Box<dyn std::error::Error + Send + Sync>,
        },
        #[error("daemon shutdown failed: {source}")]
        DaemonShutdown {
            #[source]
            source: Box<dyn std::error::Error + Send + Sync>,
        },
        #[error("could not bind {address}: {source}")]
        Bind {
            address: std::net::SocketAddr,
            #[source]
            source: io::Error,
        },
        #[error("setup cleanup failed while {operation}: {source}")]
        Cleanup {
            operation: &'static str,
            #[source]
            source: Box<MeierError>,
        },
    }

    #[derive(Debug, Error)]
    pub enum MeierError {
        #[error("{0}")]
        Cli(String),
        #[error("configuration file {path} could not be read: {source}")]
        ConfigRead { path: PathBuf, source: io::Error },
        #[error("configuration file {path} is invalid: {source}")]
        ConfigDecode {
            path: PathBuf,
            source: serde_json::Error,
        },
        #[error("configuration file {path} could not be written: {source}")]
        ConfigWrite { path: PathBuf, source: io::Error },
        #[error("{0}")]
        Io(#[from] io::Error),
        #[error("{0}")]
        Json(#[from] serde_json::Error),
        #[error("HTTP request failed with status {status}: {message}")]
        Http { status: u16, message: String },
        #[error("{0}")]
        Request(#[from] reqwest::Error),
        #[error("Lima command failed: {message}")]
        Lima { message: String },
        #[error("command `{program}` failed: {message}")]
        Command { program: String, message: String },
        #[error("command `{program}` exited with status {code}")]
        ProcessExit { program: String, code: i32 },
        #[error("setup failed: {0}")]
        Setup(#[from] SetupError),
        #[error("unsupported platform: {0}")]
        UnsupportedPlatform(String),
        #[error("{0}")]
        Validation(String),
    }

    impl MeierError {
        #[must_use]
        pub fn exit_code(&self) -> i32 {
            match self {
                Self::Cli(_) => 2,
                Self::ProcessExit { code, .. } if *code > 0 => (*code).min(255),
                _ => 1,
            }
        }
    }

    pub(crate) fn process_exit_code(status: std::process::ExitStatus) -> i32 {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            status
                .code()
                .unwrap_or_else(|| status.signal().map_or(1, |signal| 128 + signal))
        }
        #[cfg(not(unix))]
        {
            status.code().unwrap_or(1)
        }
    }

    impl From<clap::Error> for MeierError {
        fn from(error: clap::Error) -> Self {
            Self::Cli(error.to_string())
        }
    }
}

mod lima {
    use std::process::Stdio;

    #[cfg(target_os = "macos")]
    use serde_json::Value;
    use tokio::process::Command;

    use crate::{MeierError, Result, error::process_exit_code};

    fn limactl() -> String {
        std::env::var("LIMACTL").unwrap_or_else(|_| "limactl".to_owned())
    }

    #[cfg(target_os = "macos")]
    pub async fn ensure_running(instance: &str) -> Result<()> {
        tracing::debug!(%instance, "checking Lima instance state");
        let output = Command::new(limactl())
            .args(["list", "--format", "json", instance])
            .output()
            .await
            .map_err(|source| MeierError::Lima {
                message: format!("could not execute limactl: {source}"),
            })?;
        if !output.status.success() {
            return Err(MeierError::Lima {
                message: format!(
                    "instance {instance:?} is unavailable: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        let value: Value =
            serde_json::from_slice(&output.stdout).map_err(|error| MeierError::Lima {
                message: format!("limactl returned invalid JSON: {error}"),
            })?;
        let Some(status) = find_status(&value) else {
            return Err(MeierError::Lima {
                message: format!("instance {instance:?} is unavailable or missing"),
            });
        };
        if !status.eq_ignore_ascii_case("running") {
            return Err(MeierError::Lima {
                message: format!("instance {instance:?} is not running (status: {status})"),
            });
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn find_status(value: &Value) -> Option<&str> {
        match value {
            Value::Array(values) => values.iter().find_map(find_status),
            Value::Object(map) => map
                .get("status")
                .and_then(Value::as_str)
                .or_else(|| map.get("Status").and_then(Value::as_str)),
            _ => None,
        }
    }

    pub async fn capture(instance: &str, args: &[String]) -> Result<std::process::Output> {
        tracing::debug!(%instance, command = ?args.first(), "executing command through Lima");
        Command::new(limactl())
            .arg("shell")
            .arg(instance)
            .arg("--")
            .args(args)
            .output()
            .await
            .map_err(|source| MeierError::Lima {
                message: format!("could not execute limactl shell: {source}"),
            })
    }

    pub async fn passthrough(instance: &str, args: &[String]) -> Result<()> {
        tracing::debug!(%instance, command = ?args.first(), "streaming command through Lima");
        let status = Command::new(limactl())
            .arg("shell")
            .arg(instance)
            .arg("--")
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .map_err(|source| MeierError::Lima {
                message: format!("could not execute limactl shell: {source}"),
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(MeierError::ProcessExit {
                program: limactl(),
                code: process_exit_code(status),
            })
        }
    }

    #[cfg(target_os = "macos")]
    pub async fn guest_home(instance: &str) -> Result<String> {
        let output = capture(
            instance,
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf %s \"$HOME\"".to_owned(),
            ],
        )
        .await?;
        if !output.status.success() {
            return Err(MeierError::Lima {
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        String::from_utf8(output.stdout)
            .map(|value| value.trim().to_owned())
            .map_err(|error| MeierError::Lima {
                message: format!("guest HOME was not UTF-8: {error}"),
            })
    }
}

mod setup {
    //! Runtime preparation and foreground daemon orchestration.
    //!
    //! The shell scripts remain the compatibility interface.  This module mirrors
    //! their small, deterministic set of operations so the `meier` command can be
    //! used from an installed binary as well as from a checkout.

    use std::{
        fs,
        path::{Path, PathBuf},
        process::Output,
        str::FromStr,
    };

    use tokio::process::Command;

    use crate::{MeierError, Result, SetupError, config::Config, error::process_exit_code, lima};

    const FIRECRACKER_VERSION: &str = "v1.13.2";
    const FIRECRACKER_CI_VERSION: &str = "v1.13";
    const KERNEL_VERSION: &str = "6.1.141";
    const ROOTFS_VERSION: &str = "24.04";
    pub(crate) const ROOTFS_LAYOUT_VERSION: &str = "package-manager-runtime-v4";
    pub(crate) const DEFAULT_PROCESS_SPEC: &str = include_str!("../default-process.json");

    #[allow(dead_code)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Target {
        Local,
        Lima,
    }

    impl Target {
        #[tracing::instrument(skip_all, fields(instance = %instance), err)]
        async fn output(&self, instance: &str, args: &[String]) -> Result<Output> {
            let Some(program) = args.first() else {
                return Err(SetupError::Command {
                    program: "<empty>".to_owned(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "cannot execute an empty setup command",
                    ),
                }
                .into());
            };
            tracing::debug!(target = ?self, command = %program, "executing setup command");
            match self {
                Self::Local => Command::new(program)
                    .args(&args[1..])
                    .output()
                    .await
                    .map_err(|source| {
                        MeierError::Setup(SetupError::Command {
                            program: program.clone(),
                            source,
                        })
                    }),
                Self::Lima => lima::capture(instance, args).await,
            }
        }

        #[tracing::instrument(skip_all, fields(instance = %instance), err)]
        async fn checked(&self, instance: &str, args: &[String]) -> Result<Output> {
            let output = self.output(instance, args).await?;
            if output.status.success() {
                Ok(output)
            } else {
                Err(MeierError::Command {
                    program: args.first().cloned().unwrap_or_default(),
                    message: command_failure(&output),
                })
            }
        }

        #[tracing::instrument(skip_all, fields(instance = %instance), err)]
        async fn shell(&self, instance: &str, script: &str) -> Result<Output> {
            self.checked(
                instance,
                &["sh".to_owned(), "-c".to_owned(), script.to_owned()],
            )
            .await
        }

        #[tracing::instrument(skip_all, fields(instance = %instance), err)]
        async fn probe(&self, instance: &str, script: &str) -> Result<bool> {
            match self.shell(instance, script).await {
                Ok(_) => Ok(true),
                Err(MeierError::Command { .. }) => Ok(false),
                Err(error) => Err(error),
            }
        }

        #[tracing::instrument(skip_all, fields(instance = %instance), err)]
        async fn passthrough(&self, instance: &str, args: &[String]) -> Result<()> {
            let Some(program) = args.first() else {
                return Err(SetupError::Command {
                    program: "<empty>".to_owned(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "cannot stream an empty setup command",
                    ),
                }
                .into());
            };
            tracing::debug!(target = ?self, command = %program, "streaming setup command");
            match self {
                Self::Local => {
                    let status = Command::new(program)
                        .args(&args[1..])
                        .stdin(std::process::Stdio::inherit())
                        .stdout(std::process::Stdio::inherit())
                        .stderr(std::process::Stdio::inherit())
                        .status()
                        .await
                        .map_err(|source| {
                            MeierError::Setup(SetupError::Command {
                                program: program.clone(),
                                source,
                            })
                        })?;
                    if status.success() {
                        Ok(())
                    } else {
                        Err(MeierError::ProcessExit {
                            program: program.clone(),
                            code: process_exit_code(status),
                        })
                    }
                }
                Self::Lima => lima::passthrough(instance, args).await,
            }
        }

        #[tracing::instrument(skip_all, fields(instance = %instance), err)]
        async fn check_dependencies(&self, instance: &str) -> Result<()> {
            const COMMANDS: &[&str] = &[
                "cargo",
                "chmod",
                "cmp",
                "curl",
                "grep",
                "id",
                "install",
                "mkdir",
                "mkfs.ext4",
                "mktemp",
                "mv",
                "rm",
                "sha256sum",
                "ssh-keygen",
                "sudo",
                "tar",
                "tee",
                "truncate",
                "uname",
                "unsquashfs",
            ];
            for command in COMMANDS {
                let output = self
                    .output(
                        instance,
                        &[
                            "sh".to_owned(),
                            "-c".to_owned(),
                            format!("command -v {} >/dev/null 2>&1", quote(command)),
                        ],
                    )
                    .await?;
                if !output.status.success() {
                    return Err(SetupError::MissingCommand {
                        command: (*command).to_owned(),
                    }
                    .into());
                }
            }
            Ok(())
        }

        #[tracing::instrument(skip_all, fields(instance = %instance), err)]
        async fn architecture(&self, instance: &str) -> Result<Architecture> {
            let output = self
                .checked(instance, &["uname".to_owned(), "-m".to_owned()])
                .await?;
            let value = String::from_utf8_lossy(&output.stdout);
            Architecture::from_str(value.trim())
        }

        #[tracing::instrument(skip_all, fields(instance = %instance, destination = %destination), err)]
        async fn download(&self, instance: &str, url: &str, destination: &str) -> Result<()> {
            let temporary = format!("{destination}.part");
            let script = format!(
                "set -eu; temporary_path={temporary}; trap 'rm -f -- \"$temporary_path\"' EXIT INT TERM; rm -f -- \"$temporary_path\"; curl --fail --location --retry 3 --output \"$temporary_path\" {url}; mv -f -- \"$temporary_path\" {destination}; trap - EXIT INT TERM",
                temporary = quote(&temporary),
                url = quote(url),
                destination = quote(destination),
            );
            self.shell(instance, &script).await.map(|_| ())
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Architecture {
        Aarch64,
        X86_64,
    }

    impl Architecture {
        pub(crate) fn target_triple(self) -> &'static str {
            match self {
                Self::Aarch64 => "aarch64-unknown-linux-musl",
                Self::X86_64 => "x86_64-unknown-linux-musl",
            }
        }

        pub(crate) fn download_name(self) -> &'static str {
            match self {
                Self::Aarch64 => "aarch64",
                Self::X86_64 => "x86_64",
            }
        }
    }

    impl FromStr for Architecture {
        type Err = MeierError;

        fn from_str(value: &str) -> Result<Self> {
            match value {
                "aarch64" | "arm64" => Ok(Self::Aarch64),
                "x86_64" | "amd64" => Ok(Self::X86_64),
                _ => Err(SetupError::UnsupportedArchitecture {
                    value: value.to_owned(),
                }
                .into()),
            }
        }
    }

    #[must_use]
    fn execution_target() -> Target {
        #[cfg(target_os = "macos")]
        {
            Target::Lima
        }
        #[cfg(not(target_os = "macos"))]
        {
            Target::Local
        }
    }

    /// Prepare the runtime used by Barbirolli and its guest entrypoint.
    #[tracing::instrument(skip(config), err)]
    pub async fn setup(config: &Config) -> Result<()> {
        let target = execution_target();
        let instance = &config.lima.instance;
        #[cfg(target_os = "macos")]
        if matches!(target, Target::Lima) {
            lima::ensure_running(instance).await?;
        }

        let target_paths = target_paths(config)?;
        tracing::info!(runtime = %target_paths.root, target = ?target, "starting daemon setup");
        target.check_dependencies(instance).await?;
        target_paths.create_directories(&target, instance).await?;

        target_paths.install_entrypoint(&target, instance).await?;
        target_paths
            .install_daemon_binary(&target, instance)
            .await?;

        if target_paths.runtime_is_prepared(&target, instance).await? {
            tracing::info!(runtime = %target_paths.root, "daemon runtime is already prepared");
            return Ok(());
        }

        target_paths.prepare_firecracker(&target, instance).await?;
        target_paths.prepare_key(&target, instance).await?;
        target_paths.prepare_kernel(&target, instance).await?;
        target_paths.prepare_rootfs(&target, instance).await?;

        if !target_paths.runtime_is_prepared(&target, instance).await? {
            return Err(SetupError::ArtifactsIncomplete.into());
        }
        tracing::info!(runtime = %target_paths.root, "daemon runtime prepared");
        Ok(())
    }

    /// Start the feature-enabled daemon in the foreground.
    #[tracing::instrument(skip(config), fields(config_path = %config_path.display()), err)]
    pub async fn run_daemon(config: &Config, config_path: &Path) -> Result<()> {
        let target = execution_target();
        let instance = &config.lima.instance;
        #[cfg(target_os = "macos")]
        if matches!(target, Target::Lima) {
            lima::ensure_running(instance).await?;
        }
        let target_paths = target_paths(config)?;
        if !target_paths.runtime_is_prepared(&target, instance).await? {
            return Err(SetupError::RuntimeArtifactsMissing.into());
        }
        let kvm = vec!["test".to_owned(), "-e".to_owned(), "/dev/kvm".to_owned()];
        match target.checked(instance, &kvm).await {
            Ok(_) => {}
            Err(MeierError::Command { .. }) => {
                return Err(SetupError::KvmUnavailable.into());
            }
            Err(error) => return Err(error),
        }

        tracing::info!(target = ?target, "starting Meier daemon mode");
        match target {
            Target::Local => {
                let mut args = vec![
                    target_paths.daemon_binary.clone(),
                    "--config".to_owned(),
                    config_path.display().to_string(),
                    "__daemon".to_owned(),
                    "--runtime-dir".to_owned(),
                    target_paths.root.clone(),
                ];
                if unsafe { libc::geteuid() } != 0 {
                    args.insert(0, "--preserve-env=RUST_LOG".to_owned());
                    args.insert(0, "sudo".to_owned());
                }
                target.passthrough(instance, &args).await
            }
            #[cfg(target_os = "macos")]
            Target::Lima => {
                // The guest owns its config file.  This avoids trying to pass a
                // host-only path through Lima while retaining the same values.
                let guest_config = "$HOME/.meier/config.json";
                let config_json = serde_json::to_string(config)?;
                let script = format!(
                    "umask 077; mkdir -p \"$HOME/.meier\"; printf '%s\\n' {} > {guest_config}; exec sudo --preserve-env=RUST_LOG {} --config {guest_config} __daemon --runtime-dir {}",
                    quote(&config_json),
                    quote(&target_paths.daemon_binary),
                    quote(&target_paths.root),
                );
                target
                    .passthrough(instance, &["sh".to_owned(), "-c".to_owned(), script])
                    .await
            }
            #[cfg(not(target_os = "macos"))]
            Target::Lima => Err(MeierError::UnsupportedPlatform(
                "Lima execution is only available on macOS".to_owned(),
            )),
        }
    }

    #[derive(Debug, Clone)]
    pub(crate) struct TargetPaths {
        root: String,
        downloads: String,
        pub(crate) images: String,
        vms: String,
        pub(crate) ssh: String,
        firecracker: String,
        entrypoint: String,
        daemon_binary: String,
        kernel: String,
        pub(crate) rootfs: String,
        squashfs: String,
        pub(crate) authorized_keys: String,
        private_key: String,
        pub(crate) rootfs_key_marker: String,
        pub(crate) rootfs_resolver_marker: String,
        pub(crate) rootfs_layout_marker: String,
        pub(crate) rootfs_process_marker: String,
    }

    #[tracing::instrument(skip_all, err)]
    pub(crate) fn target_paths(config: &Config) -> Result<TargetPaths> {
        #[cfg(target_os = "macos")]
        let root = if config.daemon.runtime_dir == "~" {
            "$HOME".to_owned()
        } else if let Some(rest) = config.daemon.runtime_dir.strip_prefix("~/") {
            format!("$HOME/{rest}")
        } else {
            config.daemon.runtime_dir.clone()
        };
        #[cfg(not(target_os = "macos"))]
        let root = config.runtime_paths()?.root.display().to_string();

        let join = |name: &str| format!("{root}/{name}");
        let images = join("images");
        let downloads = join("downloads");
        let ssh = join("ssh");
        let rootfs = format!("{images}/ubuntu-{ROOTFS_VERSION}.ext4");
        Ok(TargetPaths {
            root: root.clone(),
            downloads: downloads.clone(),
            images: images.clone(),
            vms: join("vms"),
            ssh: ssh.clone(),
            firecracker: join("bin/firecracker"),
            entrypoint: join("bin/barbirolli_entrypoint"),
            daemon_binary: join("bin/meier"),
            kernel: format!("{images}/vmlinux"),
            rootfs: rootfs.clone(),
            squashfs: format!("{downloads}/ubuntu-{ROOTFS_VERSION}.squashfs"),
            authorized_keys: format!("{ssh}/id_ed25519.pub"),
            private_key: format!("{ssh}/id_ed25519"),
            rootfs_key_marker: format!("{rootfs}.authorized_key"),
            rootfs_resolver_marker: format!("{rootfs}.resolver"),
            rootfs_layout_marker: format!("{rootfs}.layout"),
            rootfs_process_marker: format!("{rootfs}.process.json"),
        })
    }

    impl TargetPaths {
        #[tracing::instrument(skip_all, fields(instance = %instance, runtime = %self.root))]
        async fn process_spec_matches(&self, target: Target, instance: &str) -> Result<bool> {
            if matches!(target, Target::Local) {
                return match fs::read_to_string(&self.rootfs_process_marker) {
                    Ok(contents) => Ok(contents == DEFAULT_PROCESS_SPEC),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(error.into()),
                };
            }
            let temporary = format!("{}/.meier-process-check", self.root);
            let script = format!(
                "set -eu; spec={temporary}; trap 'rm -f -- \"$spec\"' EXIT INT TERM; cat > \"$spec\" <<'MEIER_PROCESS_SPEC'\n{}\nMEIER_PROCESS_SPEC\ncmp -s {} \"$spec\"",
                DEFAULT_PROCESS_SPEC.trim_end(),
                quote(&self.rootfs_process_marker),
                temporary = quote(&temporary),
            );
            target.probe(instance, &script).await
        }
    }

    impl TargetPaths {
        #[tracing::instrument(skip_all, fields(instance = %instance, runtime = %self.root), err)]
        async fn create_directories(&self, target: &Target, instance: &str) -> Result<()> {
            let script = format!(
                "mkdir -p {} {} {} {} {}",
                quote(&format!("{}/bin", self.root)),
                quote(&self.downloads),
                quote(&self.images),
                quote(&self.vms),
                quote(&self.ssh),
            );
            target.shell(instance, &script).await.map(|_| ())
        }

        #[tracing::instrument(skip_all, fields(instance = %instance, runtime = %self.root), err)]
        async fn install_entrypoint(&self, target: &Target, instance: &str) -> Result<()> {
            let target_triple = target
                .architecture(instance)
                .await?
                .target_triple()
                .to_owned();
            let command = match workspace_root()? {
                Some(workspace) => format!(
                    "target_dir=\"${{CARGO_TARGET_DIR:-target}}\"; cd {} && cargo build --locked --release -p barbirolli_entrypoint --target {} && install -m 0755 \"$target_dir/{}/release/barbirolli_entrypoint\" {}",
                    quote(&workspace.display().to_string()),
                    quote(&target_triple),
                    quote(&target_triple),
                    quote(&self.entrypoint),
                ),
                None => packaged_entrypoint_install_command(&self.root, &target_triple),
            };
            target.shell(instance, &command).await.map(|_| ())
        }

        #[tracing::instrument(skip_all, fields(instance = %instance, runtime = %self.root), err)]
        async fn install_daemon_binary(&self, target: &Target, instance: &str) -> Result<()> {
            let workspace = workspace_root()?;
            let daemon_exists = if workspace.is_none() {
                target
                    .output(
                        instance,
                        &[
                            "sh".to_owned(),
                            "-c".to_owned(),
                            format!(
                                "test -x {} && {} --version 2>/dev/null | grep -q '^meier {}$' && {} __daemon --help >/dev/null 2>&1",
                                quote(&self.daemon_binary),
                                quote(&self.daemon_binary),
                                env!("CARGO_PKG_VERSION"),
                                quote(&self.daemon_binary),
                            ),
                        ],
                    )
                    .await?
                    .status
                    .success()
            } else {
                false
            };
            let Some(command) =
                daemon_install_command(workspace.as_deref(), &self.root, daemon_exists)
            else {
                return Ok(());
            };
            target.shell(instance, &command).await.map(|_| ())
        }

        #[tracing::instrument(skip_all, fields(instance = %instance, runtime = %self.root), err)]
        async fn runtime_is_prepared(&self, target: &Target, instance: &str) -> Result<bool> {
            let script = format!(
                "test -x {} && {} --version 2>/dev/null | grep -q '^meier {}$' && {} __daemon --help >/dev/null 2>&1 && test -x {} && {} --version 2>/dev/null | grep -q '^Firecracker v1\\.13\\.' && test -s {} && test -x {} && test -s {} && test -s {} && test -s {} && cmp -s {} {} && grep -qx 'nameserver 1.1.1.1' {} && grep -qx {} {} && test -s {}",
                quote(&self.daemon_binary),
                quote(&self.daemon_binary),
                env!("CARGO_PKG_VERSION"),
                quote(&self.daemon_binary),
                quote(&self.firecracker),
                quote(&self.firecracker),
                quote(&self.kernel),
                quote(&self.entrypoint),
                quote(&self.rootfs),
                quote(&self.private_key),
                quote(&self.authorized_keys),
                quote(&self.authorized_keys),
                quote(&self.rootfs_key_marker),
                quote(&self.rootfs_resolver_marker),
                quote(ROOTFS_LAYOUT_VERSION),
                quote(&self.rootfs_layout_marker),
                quote(&self.rootfs_process_marker),
            );
            let output = target
                .output(instance, &["sh".to_owned(), "-c".to_owned(), script])
                .await?;
            if !output.status.success() {
                return Ok(false);
            }
            self.process_spec_matches(*target, instance).await
        }
    }

    impl TargetPaths {
        #[tracing::instrument(skip_all, fields(instance = %instance, runtime = %self.root), err)]
        async fn prepare_firecracker(&self, target: &Target, instance: &str) -> Result<()> {
            let compatible = target
            .probe(
                instance,
                &format!(
                    "test -x {} && {} --version 2>/dev/null | grep -q '^Firecracker v1\\.13\\.'",
                    quote(&self.firecracker),
                    quote(&self.firecracker),
                ),
            )
            .await?;
            if compatible {
                return Ok(());
            }
            let architecture = target.architecture(instance).await?;
            let arch = architecture.download_name();
            let archive = format!("firecracker-{FIRECRACKER_VERSION}-{arch}.tgz");
            let base = format!(
                "https://github.com/firecracker-microvm/firecracker/releases/download/{FIRECRACKER_VERSION}"
            );
            target
                .download(
                    instance,
                    &format!("{base}/{archive}"),
                    &format!("{}/{}", self.downloads, archive),
                )
                .await?;
            target
                .download(
                    instance,
                    &format!("{base}/{archive}.sha256.txt"),
                    &format!("{}/{}.sha256.txt", self.downloads, archive),
                )
                .await?;
            target
                .shell(
                    instance,
                    &format!(
                        "cd {} && sha256sum -c {}",
                        quote(&self.downloads),
                        quote(&format!("{archive}.sha256.txt")),
                    ),
                )
                .await?;
            let extraction_template = format!("{}/.firecracker.XXXXXX", self.root);
            let source = format!(
                "$temporary/release-{FIRECRACKER_VERSION}-{arch}/firecracker-{FIRECRACKER_VERSION}-{arch}"
            );
            let script = format!(
                "set -eu; temporary=$(mktemp -d {template}); trap 'rm -rf -- \"$temporary\"' EXIT INT TERM; tar -xzf {archive} -C \"$temporary\"; install -m 0755 {source} {firecracker}; trap - EXIT INT TERM; rm -rf -- \"$temporary\"",
                template = quote(&extraction_template),
                archive = quote(&format!("{}/{}", self.downloads, archive)),
                source = source,
                firecracker = quote(&self.firecracker),
            );
            target.shell(instance, &script).await.map(|_| ())
        }
    }

    impl TargetPaths {
        #[tracing::instrument(skip_all, fields(instance = %instance, runtime = %self.root), err)]
        async fn prepare_key(&self, target: &Target, instance: &str) -> Result<()> {
            let present = target
                .probe(
                    instance,
                    &format!(
                        "test -s {} && test -s {}",
                        quote(&self.private_key),
                        quote(&self.authorized_keys),
                    ),
                )
                .await?;
            if present {
                return Ok(());
            }
            let either_exists = target
                .probe(
                    instance,
                    &format!(
                        "test -e {} || test -e {}",
                        quote(&self.private_key),
                        quote(&self.authorized_keys),
                    ),
                )
                .await?;
            if either_exists {
                return Err(SetupError::IncompleteSshKey.into());
            }
            target
                .checked(
                    instance,
                    &[
                        "ssh-keygen".to_owned(),
                        "-q".to_owned(),
                        "-t".to_owned(),
                        "ed25519".to_owned(),
                        "-N".to_owned(),
                        String::new(),
                        "-f".to_owned(),
                        self.private_key.clone(),
                    ],
                )
                .await
                .map(|_| ())
        }
    }

    impl TargetPaths {
        #[tracing::instrument(skip_all, fields(instance = %instance, runtime = %self.root), err)]
        async fn prepare_kernel(&self, target: &Target, instance: &str) -> Result<()> {
            let present = target
                .probe(instance, &format!("test -s {}", quote(&self.kernel)))
                .await?;
            if present {
                return Ok(());
            }
            let architecture = target.architecture(instance).await?;
            let url = format!(
                "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/{FIRECRACKER_CI_VERSION}/{}/vmlinux-{KERNEL_VERSION}",
                architecture.download_name()
            );
            target.download(instance, &url, &self.kernel).await
        }
    }

    impl TargetPaths {
        #[tracing::instrument(skip_all, fields(instance = %instance, runtime = %self.root), err)]
        pub(crate) async fn rootfs_is_prepared(
            &self,
            target: &Target,
            instance: &str,
        ) -> Result<bool> {
            let markers_match = target
                .probe(
                    instance,
                    &format!(
                        "test -s {} && cmp -s {} {} && grep -qx 'nameserver 1.1.1.1' {} && grep -qx {} {}",
                        quote(&self.rootfs),
                        quote(&self.authorized_keys),
                        quote(&self.rootfs_key_marker),
                        quote(&self.rootfs_resolver_marker),
                        quote(ROOTFS_LAYOUT_VERSION),
                        quote(&self.rootfs_layout_marker),
                    ),
                )
                .await?;
            if !markers_match {
                return Ok(false);
            }
            self.process_spec_matches(*target, instance).await
        }

        #[tracing::instrument(skip_all, fields(instance = %instance, runtime = %self.root), err)]
        async fn prepare_rootfs(&self, target: &Target, instance: &str) -> Result<()> {
            if self.rootfs_is_prepared(target, instance).await? {
                return Ok(());
            }
            let squashfs_present = target
                .probe(instance, &format!("test -s {}", quote(&self.squashfs)))
                .await?;
            if !squashfs_present {
                let architecture = target.architecture(instance).await?;
                let url = format!(
                    "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/{FIRECRACKER_CI_VERSION}/{}/ubuntu-{ROOTFS_VERSION}.squashfs",
                    architecture.download_name()
                );
                target.download(instance, &url, &self.squashfs).await?;
            }
            let work = format!("{}/.rootfs-work", self.root);
            let temporary = format!("{}/.ubuntu-{ROOTFS_VERSION}.ext4.tmp", self.images);
            let process = format!("{}/.meier-process-spec", self.root);
            let script = format!(
                "set -eu; sudo rm -rf {work}; mkdir -p {work}; sudo unsquashfs -d {work}/rootfs {squashfs}; sudo install -D -m 0600 {authorized} {work}/rootfs/root/.ssh/authorized_keys; sudo install -D -m 0644 {process} {work}/rootfs/etc/barbirolli/process.json; sudo chmod 1777 {work}/rootfs/tmp; printf 'nameserver 1.1.1.1\\n' | sudo tee {work}/rootfs/etc/resolv.conf >/dev/null; apt_line=$(grep -m1 '^_apt:' {work}/rootfs/etc/passwd); apt_fields=${{apt_line#*:}}; apt_fields=${{apt_fields#*:}}; apt_uid=${{apt_fields%%:*}}; apt_fields=${{apt_fields#*:}}; apt_gid=${{apt_fields%%:*}}; test -n \"$apt_uid\" && test -n \"$apt_gid\"; sudo chown -R root:root {work}/rootfs; sudo install -d -m 0755 {work}/rootfs/var/cache/apt/archives; sudo install -d -m 0700 {work}/rootfs/var/cache/apt/archives/partial; sudo chown \"$apt_uid:$apt_gid\" {work}/rootfs/var/cache/apt/archives/partial; sudo install -d -m 0755 {work}/rootfs/var/log/apt; sudo install -m 0644 /dev/null {work}/rootfs/var/log/dpkg.log; rm -f {temporary}; truncate -s 1G {temporary}; sudo mkfs.ext4 -F -d {work}/rootfs {temporary}; sudo chown \"$(id -u):$(id -g)\" {temporary}; mv -f {temporary} {rootfs}; install -m 0644 {authorized} {key_marker}; printf 'nameserver 1.1.1.1\\n' > {resolver_marker}; printf '%s\\n' {layout} > {layout_marker}; install -m 0644 {process} {process_marker}; sudo rm -rf {work}",
                work = quote(&work),
                squashfs = quote(&self.squashfs),
                authorized = quote(&self.authorized_keys),
                process = quote(&process),
                temporary = quote(&temporary),
                rootfs = quote(&self.rootfs),
                key_marker = quote(&self.rootfs_key_marker),
                resolver_marker = quote(&self.rootfs_resolver_marker),
                layout = quote(ROOTFS_LAYOUT_VERSION),
                layout_marker = quote(&self.rootfs_layout_marker),
                process_marker = quote(&self.rootfs_process_marker),
            );
            let transfer = process_spec_transfer(&process);
            if let Err(primary) = target.shell(instance, &transfer).await {
                return match self.remove_process_spec(target, instance, &process).await {
                    Ok(()) => Err(primary),
                    Err(cleanup) => {
                        tracing::error!(%primary, %cleanup, "rootfs process specification cleanup failed");
                        Err(SetupError::Cleanup {
                            operation: "removing the process specification",
                            source: Box::new(cleanup),
                        }
                        .into())
                    }
                };
            }

            let result = target.shell(instance, &script).await.map(|_| ());
            let partial_cleanup = if result.is_err() {
                self.remove_partial_rootfs(target, instance, &work, &temporary)
                    .await
            } else {
                Ok(())
            };
            let process_cleanup = self.remove_process_spec(target, instance, &process).await;
            if let Err(cleanup) = partial_cleanup {
                if let Err(primary) = result.as_ref() {
                    tracing::error!(%primary, %cleanup, "rootfs preparation cleanup failed");
                } else {
                    tracing::error!(%cleanup, "rootfs cleanup failed");
                }
                return Err(SetupError::Cleanup {
                    operation: "removing partial rootfs artifacts",
                    source: Box::new(cleanup),
                }
                .into());
            }
            if let Err(cleanup) = process_cleanup {
                if let Err(primary) = result.as_ref() {
                    tracing::error!(%primary, %cleanup, "rootfs process specification cleanup failed");
                } else {
                    tracing::error!(%cleanup, "rootfs process specification cleanup failed");
                }
                return Err(SetupError::Cleanup {
                    operation: "removing the process specification",
                    source: Box::new(cleanup),
                }
                .into());
            }
            result
        }

        #[tracing::instrument(skip_all, fields(instance = %instance, runtime = %self.root), err)]
        async fn remove_process_spec(
            &self,
            target: &Target,
            instance: &str,
            process: &str,
        ) -> Result<()> {
            target
                .shell(instance, &format!("rm -f {}", quote(process)))
                .await
                .map(|_| ())
        }

        #[tracing::instrument(skip_all, fields(instance = %instance, runtime = %self.root), err)]
        async fn remove_partial_rootfs(
            &self,
            target: &Target,
            instance: &str,
            work: &str,
            temporary: &str,
        ) -> Result<()> {
            target
                .shell(
                    instance,
                    &format!(
                        "set -eu; sudo rm -rf {}; rm -f {}",
                        quote(work),
                        quote(temporary)
                    ),
                )
                .await
                .map(|_| ())
        }
    }

    pub(crate) fn packaged_entrypoint_install_command(root: &str, target_triple: &str) -> String {
        format!(
            "set -eu; rustup target add {target}; cargo install --locked --version ={version} --root {root} --target {target} barbirolli_entrypoint",
            target = quote(target_triple),
            version = env!("CARGO_PKG_VERSION"),
            root = quote(root),
        )
    }

    pub(crate) fn daemon_install_command(
        workspace: Option<&Path>,
        root: &str,
        installed_daemon_is_current: bool,
    ) -> Option<String> {
        if let Some(workspace) = workspace {
            return Some(format!(
                "cargo install --locked --path {} --root {} --bin meier --features daemon --force",
                quote(&workspace.join("crates/meier").display().to_string()),
                quote(root),
            ));
        }
        (!installed_daemon_is_current).then(|| {
            format!(
                "cargo install --locked --version ={} --root {} --bin meier --features daemon meier",
                env!("CARGO_PKG_VERSION"),
                quote(root),
            )
        })
    }

    pub(crate) fn process_spec_transfer(destination: &str) -> String {
        format!(
            "cat > {} <<'MEIER_PROCESS_SPEC'\n{}\nMEIER_PROCESS_SPEC",
            quote(destination),
            DEFAULT_PROCESS_SPEC.trim_end(),
        )
    }

    #[tracing::instrument(skip_all, err)]
    fn workspace_root() -> Result<Option<PathBuf>> {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        for path in manifest.ancestors() {
            let manifest = path.join("Cargo.toml");
            match fs::read_to_string(&manifest) {
                Ok(contents) if contents.contains("[workspace]") => {
                    return Ok(Some(path.to_owned()));
                }
                Ok(_) => {}
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(SetupError::Workspace {
                        path: manifest,
                        source,
                    }
                    .into());
                }
            }
        }
        Ok(None)
    }

    fn command_failure(output: &Output) -> String {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if stderr.is_empty() {
            format!("process exited with {}", output.status)
        } else {
            stderr
        }
    }

    pub(crate) fn quote(value: &str) -> String {
        if value == "$HOME" {
            return "\"$HOME\"".to_owned();
        }
        if let Some(rest) = value.strip_prefix("$HOME/") {
            let escaped = rest
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('$', "\\$")
                .replace('`', "\\`");
            return format!("\"$HOME/{escaped}\"");
        }
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(feature = "daemon")]
pub mod daemon {
    //! Feature-gated Linux daemon runner used by the hidden `meier __daemon` mode.

    use std::{net::SocketAddr, path::PathBuf, time::Duration};

    use crate::{
        MeierError, Result,
        config::{self, Config},
    };

    #[cfg(target_os = "linux")]
    use crate::SetupError;

    /// The small, feature-gated settings surface shared by the daemon mode and
    /// compatibility launchers. The public CLI keeps that mode hidden behind the
    /// feature-enabled executable.
    #[derive(Debug, Clone)]
    pub struct DaemonSettings {
        pub vm_root: PathBuf,
        pub image_root: PathBuf,
        pub firecracker: PathBuf,
        pub barbirolli_entrypoint: PathBuf,
        pub listen: SocketAddr,
        pub idle_initial_interval: Duration,
        pub idle_strike_interval: Duration,
        pub idle_final_interval: Duration,
        pub idle_cpu_high_percent: f64,
        pub idle_cpu_low_percent: f64,
    }

    impl Config {
        /// Build daemon settings from the configured runtime paths.
        ///
        /// # Errors
        ///
        /// Returns an error when the runtime directory cannot be resolved.
        #[tracing::instrument(skip(self), err)]
        pub fn daemon_settings(&self) -> Result<DaemonSettings> {
            let paths = self.runtime_paths()?;
            Ok(DaemonSettings {
                vm_root: paths.vms,
                image_root: paths.images,
                firecracker: paths.firecracker,
                barbirolli_entrypoint: paths.entrypoint,
                listen: self.daemon.listen,
                idle_initial_interval: Duration::from_mins(5),
                idle_strike_interval: Duration::from_mins(1),
                idle_final_interval: Duration::from_secs(30),
                idle_cpu_high_percent: 0.5,
                idle_cpu_low_percent: 3.0,
            })
        }
    }

    /// Run the daemon using a previously loaded configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when runtime paths cannot be resolved or the daemon
    /// cannot start, serve, or shut down cleanly.
    #[cfg(target_os = "linux")]
    pub async fn run(config: Config) -> Result<()> {
        run_with_settings(config.daemon_settings()?).await
    }

    /// The daemon feature is available in the workspace compatibility binary,
    /// but the service itself has no native macOS implementation.
    ///
    /// # Errors
    ///
    /// Always returns an unsupported-platform error outside Linux.
    #[cfg(not(target_os = "linux"))]
    #[tracing::instrument(skip(_config), err)]
    pub async fn run(_config: Config) -> Result<()> {
        tracing::error!("the Meier daemon is only available on Linux");
        Err(MeierError::UnsupportedPlatform(
            "the Meier daemon must execute inside the Linux Lima guest".to_owned(),
        ))
    }

    /// Run the daemon with settings supplied by a caller that retains a
    /// compatibility configuration format, such as the root `ssh` binary.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot initialize, bind, serve, or shut
    /// down cleanly.
    #[cfg(target_os = "linux")]
    #[tracing::instrument(skip(settings), fields(addr = %settings.listen), err)]
    pub async fn run_with_settings(settings: DaemonSettings) -> Result<()> {
        tracing::info!(addr = %settings.listen, "initializing Meier daemon");
        let standard_rootfs =
            barbirolli::Rootfs::from(settings.image_root.join("ubuntu-24.04.ext4"));
        let store = barbirolli::VmStore::new(settings.vm_root.clone(), settings.image_root.clone())
            .map_err(|source| {
                MeierError::Setup(SetupError::Daemon {
                    source: Box::new(source),
                })
            })?;
        let daemon = barbirolli::Barbirolli::new(
            store,
            barbirolli::DaemonConfig {
                firecracker: settings.firecracker,
                entrypoint: settings.barbirolli_entrypoint,
                provisioning: barbirolli::ProvisioningConfig::default(),
                idle_policy: Some(barbirolli::IdlePolicy {
                    initial_interval: settings.idle_initial_interval,
                    strike_interval: settings.idle_strike_interval,
                    final_interval: settings.idle_final_interval,
                    cpu_idle_high_percent: settings.idle_cpu_high_percent,
                    cpu_active_low_percent: settings.idle_cpu_low_percent,
                }),
            },
        )
        .await
        .map_err(|source| {
            MeierError::Setup(SetupError::Daemon {
                source: Box::new(source),
            })
        })?;

        let listener = tokio::net::TcpListener::bind(settings.listen)
            .await
            .map_err(|source| {
                MeierError::Setup(SetupError::Bind {
                    address: settings.listen,
                    source,
                })
            })?;
        let server = axum::serve(listener, elhone::router(daemon.clone(), standard_rootfs));
        tracing::info!(addr = %settings.listen, "Meier HTTP service started");
        let service = tokio::select! {
            result = server => result.map_err(|source| MeierError::Setup(SetupError::DaemonService {
                source: Box::new(source),
            })),
            result = tokio::signal::ctrl_c() => result.map_err(|source| MeierError::Setup(SetupError::DaemonService {
                source: Box::new(source),
            })),
            () = daemon.autoscale() => Ok(()),
        };
        let shutdown = daemon.shutdown().await.map_err(|source| {
            MeierError::Setup(SetupError::DaemonShutdown {
                source: Box::new(source),
            })
        });
        match (service, shutdown) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(service), Ok(())) => Err(service),
            (Ok(()), Err(shutdown)) => Err(shutdown),
            (Err(service), Err(shutdown)) => {
                tracing::error!(%service, %shutdown, "daemon service and shutdown both failed");
                Err(service)
            }
        }
    }

    /// Reject a daemon launch on a platform without a native implementation.
    ///
    /// # Errors
    ///
    /// Always returns an unsupported-platform error outside Linux.
    #[cfg(not(target_os = "linux"))]
    #[tracing::instrument(skip(_settings), err)]
    pub async fn run_with_settings(_settings: DaemonSettings) -> Result<()> {
        tracing::error!("the Meier daemon is only available on Linux");
        Err(MeierError::UnsupportedPlatform(
            "the Meier daemon must execute inside the Linux Lima guest".to_owned(),
        ))
    }

    /// Load the launcher-selected configuration and run the daemon in the
    /// foreground. This is called by the hidden command in the same executable as
    /// the public client commands.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration cannot be loaded or the daemon
    /// cannot run.
    #[tracing::instrument(skip(runtime_dir), fields(config_path = %config_path.display()), err)]
    pub async fn run_from_cli(config_path: PathBuf, runtime_dir: Option<PathBuf>) -> Result<()> {
        let mut config = config::load(&config_path)?;
        if let Some(runtime_dir) = runtime_dir {
            config.daemon.runtime_dir = runtime_dir.display().to_string();
        }
        run(config).await
    }
}

pub use error::{MeierError, SetupError};

pub type Result<T, E = MeierError> = std::result::Result<T, E>;

/// Build the structured error document emitted by the `meier` binary.
#[must_use]
pub fn error_body(error: &MeierError) -> serde_json::Value {
    serde_json::json!({"error": error.to_string()})
}

fn tracing_filter(directives: Option<&str>) -> Result<tracing_subscriber::EnvFilter> {
    match directives {
        Some(directives) => tracing_subscriber::EnvFilter::try_new(directives)
            .map_err(|error| MeierError::Validation(format!("invalid RUST_LOG filter: {error}"))),
        None => Ok(tracing_subscriber::EnvFilter::new("off")),
    }
}

/// Run the command-line application and return its structured output.
///
/// # Errors
///
/// Returns an error when tracing configuration, argument parsing, configuration
/// loading, command validation, setup, transport, or daemon execution fails.
pub async fn run() -> Result<Option<serde_json::Value>> {
    let filter = match std::env::var("RUST_LOG") {
        Ok(directives) => tracing_filter(Some(&directives))?,
        Err(std::env::VarError::NotPresent) => tracing_filter(None)?,
        Err(error) => {
            return Err(MeierError::Validation(format!(
                "could not read RUST_LOG: {error}"
            )));
        }
    };
    let tracing_result = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
    if let Err(error) = tracing_result {
        tracing::debug!(%error, "using the existing tracing subscriber");
    }

    let cli = cli::parse_args()?;
    let config_path = cli.config.clone().unwrap_or_else(config::default_path);

    #[cfg(feature = "daemon")]
    if let Some(args) = cli.internal_daemon_args() {
        daemon::run_from_cli(config_path, args.runtime_dir.clone()).await?;
        return Ok(None);
    }

    let has_json_output = cli.has_json_output();
    let value = if cli.is_bootstrap_command() {
        cli.dispatch(&config_path)?
    } else {
        let config = config::load(&config_path)?;
        cli.dispatch_with_config(&config_path, &config).await?
    };
    Ok(has_json_output.then_some(value))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use clap::Parser;
    use serde_json::json;
    use tempfile::tempdir;

    use super::{error_body, tracing_filter};
    use crate::{
        cli::{Cli, Command, ConfigCommand, CreateArgs, DaemonCommand, ImageReference, OciCommand},
        config::{self, Config},
        setup,
    };

    #[cfg(target_os = "linux")]
    use std::{str::FromStr, sync::Arc};

    #[cfg(target_os = "linux")]
    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::State,
        http::{Request, StatusCode},
        response::Response,
        routing::any,
    };
    #[cfg(target_os = "linux")]
    use tokio::sync::{Mutex as TokioMutex, oneshot};

    #[test]
    fn create_payload_uses_external_then_internal_mapping() {
        let payload = CreateArgs {
            vcpu_count: 2,
            publish: vec!["2222:22".parse().expect("mapping")],
            authorized_key_file: Vec::new(),
        }
        .payload()
        .expect("payload should be valid");
        assert_eq!(
            payload["bindings"][0],
            json!({"internal": 22, "external": 2222})
        );
    }

    #[test]
    fn image_parser_preserves_registry_name_and_tag() {
        assert_eq!(
            "library/alpine:latest".parse::<ImageReference>().unwrap(),
            ImageReference {
                name: "library/alpine".to_owned(),
                tag: "latest".to_owned(),
            }
        );
    }

    #[test]
    fn image_parser_rejects_invalid_docker_references() {
        for value in ["Alpine:latest", "alpine:", "alpine:latest!", "alpine:."] {
            assert!(value.parse::<ImageReference>().is_err(), "{value}");
        }
    }

    #[test]
    fn defaults_match_the_documented_file() {
        let encoded = serde_json::to_value(Config::default()).expect("config serializes");
        assert_eq!(encoded["lima"]["instance"], "provisioning");
        assert_eq!(encoded["client"]["url"], "http://127.0.0.1:3000");
        assert_eq!(encoded["daemon"]["listen"], "127.0.0.1:3000");
    }

    #[test]
    fn architecture_aliases_parse_before_downloads() {
        let aarch64 = "aarch64".parse::<setup::Architecture>().expect("aarch64");
        assert_eq!(
            (aarch64.download_name(), aarch64.target_triple()),
            ("aarch64", "aarch64-unknown-linux-musl"),
        );
        let arm64 = "arm64".parse::<setup::Architecture>().expect("arm64");
        assert_eq!(
            (arm64.download_name(), arm64.target_triple()),
            ("aarch64", "aarch64-unknown-linux-musl"),
        );
        let amd64 = "amd64".parse::<setup::Architecture>().expect("amd64");
        assert_eq!(
            (amd64.download_name(), amd64.target_triple()),
            ("x86_64", "x86_64-unknown-linux-musl"),
        );
        assert!("riscv64".parse::<setup::Architecture>().is_err());
    }

    #[test]
    fn shell_paths_keep_guest_home_expansion_but_quote_other_values() {
        assert_eq!(setup::quote("$HOME"), "\"$HOME\"");
        assert_eq!(
            setup::quote("$HOME/.local/share/barbirolli"),
            "\"$HOME/.local/share/barbirolli\""
        );
        assert_eq!(setup::quote("/tmp/a path"), "'/tmp/a path'");
    }

    #[test]
    fn packaged_entrypoint_install_provisions_its_musl_target_first() {
        let command = setup::packaged_entrypoint_install_command(
            "/tmp/meier runtime",
            "x86_64-unknown-linux-musl",
        );
        let rustup = command
            .find("rustup target add 'x86_64-unknown-linux-musl'")
            .expect("target provisioning command");
        let install = command
            .find("cargo install")
            .expect("entrypoint installation command");
        assert!(rustup < install);
    }

    #[test]
    fn workspace_daemon_install_is_forced_even_when_the_version_matches() {
        let workspace = Path::new("/tmp/meier-workspace");
        let command = setup::daemon_install_command(Some(workspace), "/tmp/runtime", true)
            .expect("workspace builds must refresh the daemon");
        assert!(command.contains("--path '/tmp/meier-workspace/crates/meier'"));
        assert!(command.contains("--force"));
        assert!(
            setup::daemon_install_command(None, "/tmp/runtime", true).is_none(),
            "published binaries may reuse an immutable matching version"
        );
    }

    #[test]
    fn process_spec_transfer_writes_the_exact_json_document() {
        let directory = tempdir().expect("temporary directory");
        let destination = directory.path().join("process.json");
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(setup::process_spec_transfer(
                destination.to_str().expect("UTF-8 temporary path"),
            ))
            .output()
            .expect("process specification transfer");
        assert!(
            output.status.success(),
            "transfer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(destination).expect("transferred process specification"),
            setup::DEFAULT_PROCESS_SPEC
        );
    }

    #[tokio::test]
    async fn rootfs_readiness_rejects_a_stale_process_marker() {
        let directory = tempdir().expect("temporary directory");
        let mut config = Config::default();
        config.daemon.runtime_dir = directory.path().display().to_string();
        let paths = setup::target_paths(&config).expect("runtime paths");
        fs::create_dir_all(&paths.images).expect("image directory");
        fs::create_dir_all(&paths.ssh).expect("SSH directory");
        fs::write(&paths.rootfs, b"rootfs").expect("rootfs marker");
        fs::write(&paths.authorized_keys, b"authorized key\n").expect("authorized key");
        fs::write(&paths.rootfs_key_marker, b"authorized key\n").expect("authorized key marker");
        fs::write(&paths.rootfs_resolver_marker, b"nameserver 1.1.1.1\n").expect("resolver marker");
        fs::write(
            &paths.rootfs_layout_marker,
            format!("{}\n", setup::ROOTFS_LAYOUT_VERSION),
        )
        .expect("layout marker");
        fs::write(&paths.rootfs_process_marker, b"stale\n").expect("process marker");
        assert!(
            !paths
                .rootfs_is_prepared(&setup::Target::Local, "unused")
                .await
                .expect("stale rootfs readiness check")
        );
        fs::write(&paths.rootfs_process_marker, setup::DEFAULT_PROCESS_SPEC)
            .expect("current process marker");
        assert!(
            paths
                .rootfs_is_prepared(&setup::Target::Local, "unused")
                .await
                .expect("current rootfs readiness check")
        );
    }

    #[test]
    fn parses_the_complete_nested_command_surface() {
        let cases = [
            (vec!["meier", "config", "init"], "config"),
            (vec!["meier", "daemon", "setup"], "daemon"),
            (vec!["meier", "daemon", "run"], "daemon"),
            (vec!["meier", "vm", "create", "--vcpu-count", "2"], "vm"),
            (vec!["meier", "vm", "ps"], "vm"),
            (vec!["meier", "vm", "show", "0"], "vm"),
            (vec!["meier", "vm", "status", "0"], "vm"),
            (vec!["meier", "vm", "start", "0"], "vm"),
            (vec!["meier", "vm", "shutdown", "0"], "vm"),
            (vec!["meier", "vm", "delete", "0"], "vm"),
            (vec!["meier", "vm", "ssh", "0", "--", "uname", "-a"], "vm"),
            (vec!["meier", "vm", "logs", "0", "--pull"], "vm"),
            (vec!["meier", "vm", "logs", "0", "-p"], "vm"),
            (vec!["meier", "oci", "pull", "alpine:latest"], "oci"),
            (vec!["meier", "oci", "run", "alpine:latest"], "oci"),
            (vec!["meier", "oci", "stop", "alpine:latest"], "oci"),
            (vec!["meier", "oci", "rm", "alpine:latest"], "oci"),
        ];

        for (argv, expected) in cases {
            let cli = Cli::try_parse_from(argv).expect("command should parse");
            match (expected, cli.command) {
                ("config", Command::Config(ConfigCommand::Init))
                | ("daemon", Command::Daemon(DaemonCommand::Setup | DaemonCommand::Run))
                | ("vm", Command::Vm(_))
                | (
                    "oci",
                    Command::Oci(
                        OciCommand::Pull(_)
                        | OciCommand::Run(_)
                        | OciCommand::Stop(_)
                        | OciCommand::Rm(_),
                    ),
                ) => {}
                _ => panic!("parsed command was routed to the wrong namespace"),
            }
        }
    }

    #[test]
    fn rejects_invalid_vm_ids_and_conflicting_log_modes() {
        assert!(Cli::try_parse_from(["meier", "vm", "status", "16384"]).is_err());
        assert!(Cli::try_parse_from(["meier", "vm", "logs", "0", "--attach", "--pull"]).is_err());
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn parses_the_hidden_daemon_mode_in_the_same_binary() {
        let cli = Cli::try_parse_from([
            "meier",
            "--config",
            "/etc/meier/config.json",
            "__daemon",
            "--runtime-dir",
            "/var/lib/meier",
        ])
        .expect("daemon mode should parse when the feature is enabled");
        let Command::InternalDaemon(args) = cli.command else {
            panic!("expected the hidden daemon command");
        };
        assert_eq!(
            args.runtime_dir.as_deref(),
            Some(Path::new("/var/lib/meier"))
        );
    }

    #[test]
    fn init_writes_the_documented_config_and_refuses_overwrite() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("nested/meier/config.json");
        let initialized = config::init(&path).expect("config should initialize");
        assert_eq!(initialized, Config::default());
        assert_eq!(
            config::load(&path).expect("config should load"),
            Config::default()
        );
        assert!(config::init(&path).is_err(), "init must use create_new");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("config.json");
        fs::write(
            &path,
            br#"{"client":{"url":"http://127.0.0.1:3000"},"extra":true}"#,
        )
        .expect("config fixture");
        let error = config::load(&path).expect_err("unknown field should fail");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn errors_are_json_on_stderr_while_help_remains_raw() {
        let directory = tempdir().expect("temporary directory");
        let config_path = directory.path().join("config.json");
        let error = config::load(&config_path).expect_err("missing config should fail");
        let encoded = serde_json::to_vec(&error_body(&error)).expect("JSON error");
        let parsed: serde_json::Value = serde_json::from_slice(&encoded).expect("JSON error");
        assert!(parsed["error"].as_str().is_some());

        assert_eq!(
            tracing_filter(None)
                .expect("default tracing filter")
                .to_string(),
            "off"
        );
        let invalid_filter = tracing_filter(Some("[")).expect_err("invalid filter should fail");
        let encoded =
            serde_json::to_vec(&error_body(&invalid_filter)).expect("JSON tracing-filter error");
        let parsed: serde_json::Value =
            serde_json::from_slice(&encoded).expect("JSON tracing-filter error");
        assert!(
            parsed["error"]
                .as_str()
                .is_some_and(|error| error.contains("invalid RUST_LOG filter"))
        );

        let help = Cli::try_parse_from(["meier", "--help"]).expect_err("help should short-circuit");
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(help.to_string().starts_with("Manage Barbirolli"));

        let init = Cli::try_parse_from([
            "meier",
            "--config",
            config_path.to_str().expect("config path"),
            "config",
            "init",
        ])
        .expect("config init command");
        let value = init.dispatch(&config_path).expect("config init dispatch");
        assert!(fs::metadata(config_path).is_ok());
        assert!(value.is_object());
    }

    #[cfg(target_os = "macos")]
    static LIMA_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[cfg(target_os = "macos")]
    struct EnvironmentVariable {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    #[cfg(target_os = "macos")]
    impl EnvironmentVariable {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: the Lima tests serialize all environment changes with
            // `LIMA_ENV_LOCK` and restore each value before releasing it.
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for EnvironmentVariable {
        fn drop(&mut self) {
            // SAFETY: the matching lock remains held while this guard drops.
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var(self.key, previous);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "current_thread")]
    async fn macos_routes_api_calls_through_running_lima_without_guest_meier() {
        let _lock = LIMA_ENV_LOCK.lock().await;
        let directory = tempdir().expect("temporary directory");
        let script = fake_limactl(&directory.path().join("trace"));
        let _limactl = EnvironmentVariable::set("LIMACTL", &script);

        let value = Config::default()
            .connect()
            .await
            .expect("Lima client")
            .get("/vms")
            .await
            .expect("vm list");
        assert_eq!(value, json!([]));
        let trace = fs::read_to_string(directory.path().join("trace")).expect("limactl trace");
        assert!(trace.contains("list --format json provisioning"));
        assert!(trace.contains("shell provisioning -- curl"));
        assert!(!trace.contains("meier "), "the guest must not invoke meier");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "current_thread")]
    async fn macos_rejects_missing_or_stopped_lima_without_starting_it() {
        let _lock = LIMA_ENV_LOCK.lock().await;
        let directory = tempdir().expect("temporary directory");
        let script = fake_limactl(&directory.path().join("trace"));
        let _limactl = EnvironmentVariable::set("LIMACTL", &script);
        let _stopped = EnvironmentVariable::set("FAKE_STOPPED", "1");

        let Err(error) = Config::default().connect().await else {
            panic!("stopped Lima should fail");
        };
        assert!(error.to_string().contains("not running"));
        let trace = fs::read_to_string(directory.path().join("trace")).expect("limactl trace");
        assert!(
            !trace.contains("shell provisioning"),
            "stopped Lima must not receive guest work"
        );
    }

    #[cfg(target_os = "macos")]
    fn fake_limactl(trace: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let script = trace.with_file_name("limactl");
        let contents = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nif [ \"$1\" = list ]; then\n  if [ \"$FAKE_STOPPED\" = 1 ]; then printf '[{{\"status\":\"Stopped\"}}]'; else printf '[{{\"status\":\"Running\"}}]'; fi\n  exit 0\nfi\nif [ \"$1\" = shell ]; then\n  [ \"$4\" = meier ] && exit 99\n  printf '[]\\n__MEIER_HTTP_STATUS__200'\n  exit 0\nfi\nexit 64\n",
            shell_literal(&trace.display().to_string())
        );
        fs::write(&script, contents).expect("fake limactl");
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).expect("permissions");
        script
    }

    #[cfg(target_os = "macos")]
    fn shell_literal(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    #[cfg(target_os = "linux")]
    type RequestResult = std::result::Result<RequestSnapshot, String>;
    #[cfg(target_os = "linux")]
    type RequestSender = oneshot::Sender<RequestResult>;
    #[cfg(target_os = "linux")]
    type SharedRequestSender = Arc<TokioMutex<Option<RequestSender>>>;

    #[cfg(target_os = "linux")]
    #[derive(Clone)]
    struct Fixture {
        expected_method: String,
        expected_path: String,
        check_create: bool,
        response_body: String,
        result: SharedRequestSender,
    }

    #[cfg(target_os = "linux")]
    #[derive(Debug)]
    struct RequestSnapshot {
        method: String,
        path: String,
        body: String,
    }

    #[cfg(target_os = "linux")]
    struct CliRequestCase {
        method: &'static str,
        path: &'static str,
        body: &'static str,
        check_create: bool,
        command: Command,
    }

    #[cfg(target_os = "linux")]
    fn cli_request_cases() -> [CliRequestCase; 5] {
        use crate::cli::{IdArgs, ImageArgs, PortMapping, VmCommand, VmId};

        [
            CliRequestCase {
                method: "POST",
                path: "/vms",
                body: r#"{"id":0}"#,
                check_create: true,
                command: Command::Vm(VmCommand::Create(CreateArgs {
                    vcpu_count: 2,
                    publish: vec![PortMapping {
                        external: 2222,
                        internal: 22,
                    }],
                    authorized_key_file: Vec::new(),
                })),
            },
            CliRequestCase {
                method: "GET",
                path: "/vms/0/status",
                body: r#"{"status":"running"}"#,
                check_create: false,
                command: Command::Vm(VmCommand::Status(IdArgs {
                    id: VmId::from_str("0").expect("id"),
                })),
            },
            CliRequestCase {
                method: "POST",
                path: "/vms/0/shutdown",
                body: "",
                check_create: false,
                command: Command::Vm(VmCommand::Shutdown(IdArgs {
                    id: VmId::from_str("0").expect("id"),
                })),
            },
            CliRequestCase {
                method: "DELETE",
                path: "/vms/0",
                body: "",
                check_create: false,
                command: Command::Vm(VmCommand::Delete(IdArgs {
                    id: VmId::from_str("0").expect("id"),
                })),
            },
            CliRequestCase {
                method: "POST",
                path: "/oci/rm",
                body: "",
                check_create: false,
                command: Command::Oci(OciCommand::Rm(ImageArgs {
                    image: "alpine:latest".parse::<ImageReference>().expect("image"),
                })),
            },
        ]
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn maps_vm_and_oci_operations_to_elhone_requests() {
        use crate::config::ClientConfig;
        use tokio::net::TcpListener;

        let _provider_installation = rustls::crypto::ring::default_provider().install_default();
        for CliRequestCase {
            method,
            path,
            body,
            check_create,
            command,
        } in cli_request_cases()
        {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
            let address = listener.local_addr().expect("address");
            let (seen_tx, seen_rx) = oneshot::channel();
            let fixture = Fixture {
                expected_method: method.to_owned(),
                expected_path: path.to_owned(),
                check_create,
                response_body: body.to_owned(),
                result: Arc::new(TokioMutex::new(Some(seen_tx))),
            };
            let app = Router::new()
                .fallback(any(receive_request))
                .with_state(fixture.clone());
            let server = tokio::spawn(async move { axum::serve(listener, app).await });

            let config = Config {
                client: ClientConfig {
                    url: format!("http://{address}"),
                },
                ..Config::default()
            };
            let cli = Cli {
                config: None,
                command,
            };
            let value = cli
                .dispatch_with_config(Path::new("unused.json"), &config)
                .await
                .expect("request should succeed");
            let expected = if body.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_str(body).expect("fixture response should be JSON")
            };
            assert_eq!(value, expected);

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

    #[cfg(target_os = "linux")]
    async fn receive_request(
        State(fixture): State<Fixture>,
        request: Request<Body>,
    ) -> Response<Body> {
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
}
