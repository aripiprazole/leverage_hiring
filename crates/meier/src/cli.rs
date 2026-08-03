use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use clap::{Args, Parser, Subcommand, error::ErrorKind};
use serde_json::{Value, json};

use crate::{
    MeierError,
    client::{ApiClient, connect},
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
        let (name, tag) = value
            .rsplit_once(':')
            .ok_or_else(|| format!("expected a Docker image like alpine:latest, got {value:?}"))?;
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

pub fn parse_args() -> Result<Cli, MeierError> {
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
    pub async fn dispatch(&self, config_path: &Path) -> Result<(), MeierError> {
        match &self.command {
            Command::Config(ConfigCommand::Init) => {
                let config = config::init(config_path)?;
                println!("{}", serde_json::to_string_pretty(&config)?);
                Ok(())
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

    pub async fn dispatch_with_config(
        &self,
        config_path: &Path,
        config: &Config,
    ) -> Result<(), MeierError> {
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
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"status": "ready"}))?
                );
                Ok(())
            }
            Command::Daemon(DaemonCommand::Run) => setup::run_daemon(config, config_path).await,
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
    async fn dispatch(&self, config: &Config) -> Result<(), MeierError> {
        match self {
            Self::Create(args) => {
                let body = args.payload()?;
                let value = connect(config).await?.post("/vms", Some(body)).await?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            Self::Ps => {
                let value = connect(config).await?.get("/vms").await?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            Self::Show(args) => args.show_status(config, "").await,
            Self::Status(args) => args.show_status(config, "/status").await,
            Self::Start(args) => args.mutate(config, "start").await,
            Self::Shutdown(args) => args.mutate(config, "shutdown").await,
            Self::Delete(args) => {
                let value = connect(config)
                    .await?
                    .delete(&format!("/vms/{}", args.id))
                    .await?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            Self::Logs(args) => {
                let follow = args.attach || !args.pull;
                connect(config)
                    .await?
                    .stream_logs(args.id.get(), follow)
                    .await
            }
            Self::Ssh(args) => connect(config).await?.ssh(args).await,
        }
    }
}

impl IdArgs {
    async fn show_status(&self, config: &Config, suffix: &str) -> Result<(), MeierError> {
        let value = connect(config)
            .await?
            .get(&format!("/vms/{}{suffix}", self.id))
            .await?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok(())
    }

    async fn mutate(&self, config: &Config, operation: &str) -> Result<(), MeierError> {
        let value = connect(config)
            .await?
            .post(&format!("/vms/{}/{}", self.id, operation), None)
            .await?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok(())
    }
}

impl OciCommand {
    async fn dispatch(&self, config: &Config) -> Result<(), MeierError> {
        let client = connect(config).await?;
        let value = match self {
            Self::Pull(args) => {
                let token = args.image.docker_token(&client).await?;
                client
                    .post(
                        "/oci/pull",
                        Some(json!({
                            "name": &args.image.name,
                            "tag": &args.image.tag,
                            "token": token,
                        })),
                    )
                    .await?
            }
            Self::Run(args) => {
                let token = args.image.docker_token(&client).await?;
                client
                    .post(
                        "/oci/run",
                        Some(json!({
                            "name": &args.image.name,
                            "tag": &args.image.tag,
                            "token": token,
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
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok(())
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

    async fn docker_token(&self, client: &ApiClient) -> Result<String, MeierError> {
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
    fn payload(&self) -> Result<Value, MeierError> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
