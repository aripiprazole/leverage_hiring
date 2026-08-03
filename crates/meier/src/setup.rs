//! Runtime preparation and foreground daemon orchestration.
//!
//! The shell scripts remain the compatibility interface.  This module mirrors
//! their small, deterministic set of operations so the `meier` command can be
//! used from an installed binary as well as from a checkout.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Output,
};

use tokio::process::Command;

use crate::{MeierError, config::Config, error::process_exit_code, lima};

const FIRECRACKER_VERSION: &str = "v1.13.2";
const FIRECRACKER_CI_VERSION: &str = "v1.13";
const KERNEL_VERSION: &str = "6.1.141";
const ROOTFS_VERSION: &str = "24.04";
const ROOTFS_LAYOUT_VERSION: &str = "daemon-entrypoint-v1";
const DEFAULT_PROCESS_SPEC: &str = include_str!("../default-process.json");

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Local,
    Lima,
}

impl Target {
    async fn output(&self, instance: &str, args: &[String]) -> Result<Output, MeierError> {
        tracing::debug!(target = ?self, command = ?args.first(), "executing setup command");
        match self {
            Self::Local => Command::new(&args[0])
                .args(&args[1..])
                .output()
                .await
                .map_err(|source| {
                    MeierError::Setup(format!("could not execute {}: {source}", args[0]))
                }),
            Self::Lima => lima::capture(instance, args).await,
        }
    }

    async fn checked(&self, instance: &str, args: &[String]) -> Result<Output, MeierError> {
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

    async fn shell(&self, instance: &str, script: &str) -> Result<Output, MeierError> {
        self.checked(
            instance,
            &["sh".to_owned(), "-c".to_owned(), script.to_owned()],
        )
        .await
    }

    async fn passthrough(&self, instance: &str, args: &[String]) -> Result<(), MeierError> {
        tracing::debug!(target = ?self, command = ?args.first(), "streaming setup command");
        match self {
            Self::Local => {
                let status = Command::new(&args[0])
                    .args(&args[1..])
                    .stdin(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .status()
                    .await
                    .map_err(|source| {
                        MeierError::Setup(format!("could not execute {}: {source}", args[0]))
                    })?;
                if status.success() {
                    Ok(())
                } else {
                    Err(MeierError::ProcessExit {
                        program: args[0].clone(),
                        code: process_exit_code(status),
                    })
                }
            }
            Self::Lima => lima::passthrough(instance, args).await,
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
pub async fn setup(config: &Config) -> Result<(), MeierError> {
    let target = execution_target();
    let instance = &config.lima.instance;
    #[cfg(target_os = "macos")]
    if matches!(target, Target::Lima) {
        lima::ensure_running(instance).await?;
    }

    let target_paths = target_paths(config)?;
    tracing::info!(runtime = %target_paths.root, target = ?target, "starting daemon setup");
    validate_dependencies(target, instance).await?;
    create_directories(target, instance, &target_paths).await?;

    install_entrypoint(target, instance, &target_paths).await?;
    install_daemon_binary(target, instance, &target_paths).await?;

    if runtime_is_prepared(target, instance, &target_paths).await? {
        tracing::info!(runtime = %target_paths.root, "daemon runtime is already prepared");
        return Ok(());
    }

    prepare_firecracker(target, instance, &target_paths).await?;
    prepare_key(target, instance, &target_paths).await?;
    prepare_kernel(target, instance, &target_paths).await?;
    prepare_rootfs(target, instance, &target_paths).await?;

    if !runtime_is_prepared(target, instance, &target_paths).await? {
        return Err(MeierError::Setup(
            "setup finished without producing all required runtime artifacts".to_owned(),
        ));
    }
    tracing::info!(runtime = %target_paths.root, "daemon runtime prepared");
    Ok(())
}

/// Validate and foreground-execute the feature-enabled `meier` daemon mode.
#[tracing::instrument(skip(config), fields(config_path = %config_path.display()), err)]
pub async fn run_daemon(config: &Config, config_path: &Path) -> Result<(), MeierError> {
    let target = execution_target();
    let instance = &config.lima.instance;
    #[cfg(target_os = "macos")]
    if matches!(target, Target::Lima) {
        lima::ensure_running(instance).await?;
    }
    let target_paths = target_paths(config)?;
    if !runtime_is_prepared(target, instance, &target_paths).await? {
        return Err(MeierError::Setup(
            "runtime artifacts are missing or invalid; run `meier daemon setup` first".to_owned(),
        ));
    }
    let kvm = vec!["test".to_owned(), "-e".to_owned(), "/dev/kvm".to_owned()];
    target.checked(instance, &kvm).await.map_err(|_| {
        MeierError::Setup("/dev/kvm is unavailable on the execution target".to_owned())
    })?;

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
                shell_quote(&config_json),
                shell_quote(&target_paths.daemon_binary),
                shell_quote(&target_paths.root),
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
struct TargetPaths {
    root: String,
    downloads: String,
    images: String,
    vms: String,
    ssh: String,
    firecracker: String,
    entrypoint: String,
    daemon_binary: String,
    kernel: String,
    rootfs: String,
    squashfs: String,
    authorized_keys: String,
    private_key: String,
    rootfs_key_marker: String,
    rootfs_resolver_marker: String,
    rootfs_layout_marker: String,
    rootfs_process_marker: String,
}

fn target_paths(config: &Config) -> Result<TargetPaths, MeierError> {
    #[cfg(target_os = "macos")]
    let root = if config.daemon.runtime_dir == "~" {
        "$HOME".to_owned()
    } else if let Some(rest) = config.daemon.runtime_dir.strip_prefix("~/") {
        format!("$HOME/{rest}")
    } else {
        config.daemon.runtime_dir.clone()
    };
    #[cfg(not(target_os = "macos"))]
    let root = crate::config::runtime_paths(config)?
        .root
        .display()
        .to_string();

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
    async fn process_spec_matches(&self, target: Target, instance: &str) -> bool {
        if matches!(target, Target::Local) {
            return fs::read_to_string(&self.rootfs_process_marker)
                .ok()
                .as_deref()
                == Some(DEFAULT_PROCESS_SPEC);
        }
        let temporary = "/tmp/meier-default-process.json";
        let script = format!(
            "set -eu; spec={temporary}; trap 'rm -f -- \"$spec\"' EXIT INT TERM; cat > \"$spec\" <<'MEIER_PROCESS_SPEC'\n{}\nMEIER_PROCESS_SPEC\ncmp -s {} \"$spec\"",
            DEFAULT_PROCESS_SPEC.trim_end(),
            shell_quote(&self.rootfs_process_marker),
        );
        target.shell(instance, &script).await.is_ok()
    }
}

async fn validate_dependencies(target: Target, instance: &str) -> Result<(), MeierError> {
    let commands = [
        "cargo",
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
        "truncate",
        "uname",
        "unsquashfs",
    ];
    for command in commands {
        let output = target
            .output(
                instance,
                &[
                    "sh".to_owned(),
                    "-c".to_owned(),
                    format!("command -v {} >/dev/null 2>&1", shell_quote(command)),
                ],
            )
            .await?;
        if !output.status.success() {
            return Err(MeierError::Setup(format!(
                "missing required command `{command}` on the execution target"
            )));
        }
    }
    Ok(())
}

async fn create_directories(
    target: Target,
    instance: &str,
    paths: &TargetPaths,
) -> Result<(), MeierError> {
    let script = format!(
        "mkdir -p {} {} {} {} {}",
        shell_quote(&format!("{}/bin", paths.root)),
        shell_quote(&paths.downloads),
        shell_quote(&paths.images),
        shell_quote(&paths.vms),
        shell_quote(&paths.ssh),
    );
    target.shell(instance, &script).await.map(|_| ())
}

async fn install_entrypoint(
    target: Target,
    instance: &str,
    paths: &TargetPaths,
) -> Result<(), MeierError> {
    let target_arch = target_arch(target, instance).await?;
    let target_triple = static_target(&target_arch)?;
    let workspace = workspace_root();
    let command = if let Some(workspace) = workspace {
        format!(
            "target_dir=\"${{CARGO_TARGET_DIR:-target}}\"; cd {} && cargo build --release -p barbirolli_entrypoint --target {} && install -m 0755 \"$target_dir/{}/release/barbirolli_entrypoint\" {}",
            shell_quote(&workspace.display().to_string()),
            shell_quote(&target_triple),
            shell_quote(&target_triple),
            shell_quote(&paths.entrypoint),
        )
    } else {
        format!(
            "cargo install --locked --version ={} --root {} --target {} barbirolli_entrypoint",
            env!("CARGO_PKG_VERSION"),
            shell_quote(&paths.root),
            shell_quote(&target_triple),
        )
    };
    target.shell(instance, &command).await.map(|_| ())
}

async fn install_daemon_binary(
    target: Target,
    instance: &str,
    paths: &TargetPaths,
) -> Result<(), MeierError> {
    let daemon_exists = target
        .output(
            instance,
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                format!(
                    "test -x {} && {} --version 2>/dev/null | grep -q '^meier {}$'",
                    shell_quote(&paths.daemon_binary),
                    shell_quote(&paths.daemon_binary),
                    env!("CARGO_PKG_VERSION")
                ),
            ],
        )
        .await?
        .status
        .success();
    if daemon_exists {
        return Ok(());
    }
    let command = if let Some(workspace) = workspace_root() {
        format!(
            "cargo install --locked --path {} --root {} --bin meier --features daemon --force",
            shell_quote(&workspace.join("crates/meier").display().to_string()),
            shell_quote(&paths.root),
        )
    } else {
        format!(
            "cargo install --locked --version ={} --root {} --bin meier --features daemon meier",
            env!("CARGO_PKG_VERSION"),
            shell_quote(&paths.root),
        )
    };
    target.shell(instance, &command).await.map(|_| ())
}

async fn runtime_is_prepared(
    target: Target,
    instance: &str,
    paths: &TargetPaths,
) -> Result<bool, MeierError> {
    let script = format!(
        "test -x {} && {} --version 2>/dev/null | grep -q '^meier {}$' && test -x {} && {} --version 2>/dev/null | grep -q '^Firecracker v1\\.13\\.' && test -s {} && test -x {} && test -s {} && test -s {} && test -s {} && cmp -s {} {} && grep -qx 'nameserver 1.1.1.1' {} && grep -qx {} {} && test -s {}",
        shell_quote(&paths.daemon_binary),
        shell_quote(&paths.daemon_binary),
        env!("CARGO_PKG_VERSION"),
        shell_quote(&paths.firecracker),
        shell_quote(&paths.firecracker),
        shell_quote(&paths.kernel),
        shell_quote(&paths.entrypoint),
        shell_quote(&paths.rootfs),
        shell_quote(&paths.private_key),
        shell_quote(&paths.authorized_keys),
        shell_quote(&paths.authorized_keys),
        shell_quote(&paths.rootfs_key_marker),
        shell_quote(&paths.rootfs_resolver_marker),
        shell_quote(ROOTFS_LAYOUT_VERSION),
        shell_quote(&paths.rootfs_layout_marker),
        shell_quote(&paths.rootfs_process_marker),
    );
    let output = target
        .output(instance, &["sh".to_owned(), "-c".to_owned(), script])
        .await?;
    if !output.status.success() {
        return Ok(false);
    }
    Ok(paths.process_spec_matches(target, instance).await)
}

async fn prepare_firecracker(
    target: Target,
    instance: &str,
    paths: &TargetPaths,
) -> Result<(), MeierError> {
    let compatible = target
        .shell(
            instance,
            &format!(
                "test -x {} && {} --version 2>/dev/null | grep -q '^Firecracker v1\\.13\\.'",
                shell_quote(&paths.firecracker),
                shell_quote(&paths.firecracker),
            ),
        )
        .await
        .is_ok();
    if compatible {
        return Ok(());
    }
    let arch = target_arch(target, instance).await?;
    let archive = format!("firecracker-{FIRECRACKER_VERSION}-{arch}.tgz");
    let base = format!(
        "https://github.com/firecracker-microvm/firecracker/releases/download/{FIRECRACKER_VERSION}"
    );
    download(
        target,
        instance,
        &format!("{base}/{archive}"),
        &format!("{}/{}", paths.downloads, archive),
    )
    .await?;
    download(
        target,
        instance,
        &format!("{base}/{archive}.sha256.txt"),
        &format!("{}/{}.sha256.txt", paths.downloads, archive),
    )
    .await?;
    target
        .shell(
            instance,
            &format!(
                "cd {} && sha256sum -c {}",
                shell_quote(&paths.downloads),
                shell_quote(&format!("{archive}.sha256.txt")),
            ),
        )
        .await?;
    let extraction = format!("{}/.firecracker-extract", paths.root);
    let source = format!(
        "{extraction}/release-{FIRECRACKER_VERSION}-{arch}/firecracker-{FIRECRACKER_VERSION}-{arch}"
    );
    let script = format!(
        "rm -rf {} && mkdir -p {} && tar -xzf {} -C {} && install -m 0755 {} {} && rm -rf {}",
        shell_quote(&extraction),
        shell_quote(&extraction),
        shell_quote(&format!("{}/{}", paths.downloads, archive)),
        shell_quote(&extraction),
        shell_quote(&source),
        shell_quote(&paths.firecracker),
        shell_quote(&extraction),
    );
    target.shell(instance, &script).await.map(|_| ())
}

async fn prepare_key(
    target: Target,
    instance: &str,
    paths: &TargetPaths,
) -> Result<(), MeierError> {
    let present = target
        .shell(
            instance,
            &format!(
                "test -s {} && test -s {}",
                shell_quote(&paths.private_key),
                shell_quote(&paths.authorized_keys),
            ),
        )
        .await
        .is_ok();
    if present {
        return Ok(());
    }
    let either_exists = target
        .shell(
            instance,
            &format!(
                "test -e {} || test -e {}",
                shell_quote(&paths.private_key),
                shell_quote(&paths.authorized_keys),
            ),
        )
        .await
        .is_ok();
    if either_exists {
        return Err(MeierError::Setup(
            "incomplete SSH key pair; move it aside and rerun setup".to_owned(),
        ));
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
                paths.private_key.clone(),
            ],
        )
        .await
        .map(|_| ())
}

async fn prepare_kernel(
    target: Target,
    instance: &str,
    paths: &TargetPaths,
) -> Result<(), MeierError> {
    let present = target
        .shell(instance, &format!("test -s {}", shell_quote(&paths.kernel)))
        .await
        .is_ok();
    if present {
        return Ok(());
    }
    let arch = target_arch(target, instance).await?;
    let url = format!(
        "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/{FIRECRACKER_CI_VERSION}/{arch}/vmlinux-{KERNEL_VERSION}"
    );
    download(target, instance, &url, &paths.kernel).await
}

async fn prepare_rootfs(
    target: Target,
    instance: &str,
    paths: &TargetPaths,
) -> Result<(), MeierError> {
    let ready = target
        .shell(
            instance,
            &format!(
                "test -s {} && cmp -s {} {} && grep -qx 'nameserver 1.1.1.1' {} && grep -qx {} {} && test -s {}",
                shell_quote(&paths.rootfs),
                shell_quote(&paths.authorized_keys),
                shell_quote(&paths.rootfs_key_marker),
                shell_quote(&paths.rootfs_resolver_marker),
                shell_quote(ROOTFS_LAYOUT_VERSION),
                shell_quote(&paths.rootfs_layout_marker),
                shell_quote(&paths.rootfs_process_marker),
            ),
        )
        .await
        .is_ok();
    if ready {
        return Ok(());
    }
    let squashfs_present = target
        .shell(
            instance,
            &format!("test -s {}", shell_quote(&paths.squashfs)),
        )
        .await
        .is_ok();
    if !squashfs_present {
        let arch = target_arch(target, instance).await?;
        let url = format!(
            "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/{FIRECRACKER_CI_VERSION}/{arch}/ubuntu-{ROOTFS_VERSION}.squashfs"
        );
        download(target, instance, &url, &paths.squashfs).await?;
    }
    let work = format!("{}/.rootfs-work", paths.root);
    let temporary = format!("{}/.ubuntu-{ROOTFS_VERSION}.ext4.tmp", paths.images);
    let script = format!(
        "set -eu; rm -rf {work}; mkdir -p {work}; sudo unsquashfs -d {work}/rootfs {squashfs}; sudo install -D -m 0600 {authorized} {work}/rootfs/root/.ssh/authorized_keys; sudo install -D -m 0644 {process} {work}/rootfs/etc/barbirolli/process.json; printf 'nameserver 1.1.1.1\\n' | sudo tee {work}/rootfs/etc/resolv.conf >/dev/null; sudo chown -R root:root {work}/rootfs; rm -f {temporary}; truncate -s 1G {temporary}; sudo mkfs.ext4 -F -d {work}/rootfs {temporary}; sudo chown \"$(id -u):$(id -g)\" {temporary}; mv -f {temporary} {rootfs}; install -m 0644 {authorized} {key_marker}; printf 'nameserver 1.1.1.1\\n' > {resolver_marker}; printf '%s\\n' {layout} > {layout_marker}; install -m 0644 {process} {process_marker}; sudo rm -rf {work}",
        work = shell_quote(&work),
        squashfs = shell_quote(&paths.squashfs),
        authorized = shell_quote(&paths.authorized_keys),
        process = shell_quote("/tmp/meier-default-process.json"),
        temporary = shell_quote(&temporary),
        rootfs = shell_quote(&paths.rootfs),
        key_marker = shell_quote(&paths.rootfs_key_marker),
        resolver_marker = shell_quote(&paths.rootfs_resolver_marker),
        layout = shell_quote(ROOTFS_LAYOUT_VERSION),
        layout_marker = shell_quote(&paths.rootfs_layout_marker),
        process_marker = shell_quote(&paths.rootfs_process_marker),
    );
    // Put the process specification on the target without ever including it
    // in a tracing field.  The temporary file is removed by the script.
    let transfer = format!(
        "cat > /tmp/meier-default-process.json <<'MEIER_PROCESS_SPEC'\n{}\nMEIER_PROCESS_SPEC",
        DEFAULT_PROCESS_SPEC.trim_end(),
    );
    target.shell(instance, &transfer).await?;
    let result = target.shell(instance, &script).await;
    if result.is_err() {
        // Cleanup is deliberately best-effort and safe to repeat.  This also
        // prevents a failed mkfs or checksum from leaving a partial image.
        let _ = target
            .shell(
                instance,
                &format!("rm -rf {} {}", shell_quote(&work), shell_quote(&temporary)),
            )
            .await;
    }
    result.map(|_| ())
}

async fn download(
    target: Target,
    instance: &str,
    url: &str,
    destination: &str,
) -> Result<(), MeierError> {
    let temporary = format!("{destination}.part");
    let script = format!(
        "set -eu; temporary_path={temporary}; trap 'rm -f -- \"$temporary_path\"' EXIT INT TERM; rm -f -- \"$temporary_path\"; curl --fail --location --retry 3 --output \"$temporary_path\" {url}; mv -f -- \"$temporary_path\" {destination}; trap - EXIT INT TERM",
        temporary = shell_quote(&temporary),
        url = shell_quote(url),
        destination = shell_quote(destination),
    );
    target.shell(instance, &script).await.map(|_| ())
}

async fn target_arch(target: Target, instance: &str) -> Result<String, MeierError> {
    let output = target
        .checked(instance, &["uname".to_owned(), "-m".to_owned()])
        .await?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    normalize_arch(&value)
}

fn normalize_arch(value: &str) -> Result<String, MeierError> {
    match value {
        "aarch64" | "arm64" => Ok("aarch64".to_owned()),
        "x86_64" | "amd64" => Ok("x86_64".to_owned()),
        _ => Err(MeierError::Setup(format!(
            "unsupported target architecture `{value}`"
        ))),
    }
}

fn static_target(architecture: &str) -> Result<String, MeierError> {
    match architecture {
        "aarch64" => Ok("aarch64-unknown-linux-musl".to_owned()),
        "x86_64" => Ok("x86_64-unknown-linux-musl".to_owned()),
        _ => Err(MeierError::Setup(format!(
            "unsupported static target architecture `{architecture}`"
        ))),
    }
}

fn workspace_root() -> Option<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .find(|path| {
            fs::read_to_string(path.join("Cargo.toml"))
                .map(|contents| contents.contains("[workspace]"))
                .unwrap_or(false)
        })
        .map(Path::to_owned)
}

fn command_failure(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("process exited with {}", output.status)
    } else {
        stderr
    }
}

fn shell_quote(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn architecture_aliases_are_normalized_before_downloads() {
        assert_eq!(normalize_arch("aarch64").unwrap(), "aarch64");
        assert_eq!(normalize_arch("arm64").unwrap(), "aarch64");
        assert_eq!(normalize_arch("amd64").unwrap(), "x86_64");
        assert!(normalize_arch("riscv64").is_err());
        assert_eq!(
            static_target("aarch64").unwrap(),
            "aarch64-unknown-linux-musl"
        );
        assert_eq!(
            static_target("x86_64").unwrap(),
            "x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn shell_paths_keep_guest_home_expansion_but_quote_other_values() {
        assert_eq!(shell_quote("$HOME"), "\"$HOME\"");
        assert_eq!(
            shell_quote("$HOME/.local/share/barbirolli"),
            "\"$HOME/.local/share/barbirolli\""
        );
        assert_eq!(shell_quote("/tmp/a path"), "'/tmp/a path'");
    }
}
