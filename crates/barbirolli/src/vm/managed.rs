use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use derive_more::Debug;
use fctools::{
    extension::metrics::Metrics,
    process_spawner::DirectProcessSpawner,
    runtime::{Runtime, RuntimeChild, tokio::TokioRuntime},
    vm::{
        Vm, VmError as FctoolsVmError, VmState,
        api::{VmApi, VmApiError},
        configuration::{InitMethod, VmConfiguration, VmConfigurationData},
        models::{
            BalloonDevice, BalloonStatistics, BootSource, Drive, MachineConfiguration,
            MetricsSystem, NetworkInterface, UpdateBalloonDevice,
        },
        shutdown::{VmShutdownAction, VmShutdownError, VmShutdownMethod},
    },
    vmm::{
        arguments::{
            VmmApiSocket, VmmArguments,
            jailer::{JailerArguments, JailerCgroupVersion},
        },
        executor::{
            either::EitherVmmExecutor,
            jailed::{FlatVirtualPathResolver, JailedVmmExecutor},
            unrestricted::UnrestrictedVmmExecutor,
        },
        id::{VmmId, VmmIdError},
        installation::VmmInstallation,
        ownership::VmmOwnershipModel,
        resource::{
            CreatedResourceType, MovedResourceType, ResourceType,
            system::{ResourceSystem, ResourceSystemError},
        },
    },
};
use futures::{AsyncReadExt, AsyncWriteExt, FutureExt, future::BoxFuture};
use nonempty_collections::NEVec;
use rustix::process::Pid;
use tokio::{
    fs::OpenOptions,
    io::{AsyncBufReadExt, BufReader},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tracing::{Instrument as _, info_span};
use validated::Validated;

use crate::{
    Barbirolli, FirecrackerExecutorConfig, VmId, VmSpec,
    idle::{Monitor, Sample},
    io_error,
    lifecycle::PidError,
    network::{ManagedNetwork, NetworkError},
};

type FirecrackerResourceSystem = ResourceSystem<DirectProcessSpawner, TokioRuntime>;
type FirecrackerVm =
    Vm<EitherVmmExecutor<FlatVirtualPathResolver>, DirectProcessSpawner, TokioRuntime>;
type FirecrackerChild = <TokioRuntime as Runtime>::Child;
type FirecrackerStdout = <FirecrackerChild as RuntimeChild>::Stdout;
type FirecrackerStderr = <FirecrackerChild as RuntimeChild>::Stderr;
type FirecrackerStdin = <FirecrackerChild as RuntimeChild>::Stdin;
type Result<T = (), E = LifecycleError> = std::result::Result<T, E>;

#[derive(Debug)]
pub struct ManagedVm {
    pub spec: VmSpec,
    pub failed: bool,
    pub pid: Pid,
    pub monitor: Mutex<Option<Monitor>>,
    network: Option<ManagedNetwork>,
    #[debug(skip)]
    vm: FirecrackerVm,
    cgroup: Option<VmCgroup>,
    metrics: Arc<RwLock<Option<Metrics>>>,
    #[debug(skip)]
    metrics_task: JoinHandle<()>,
    console: SerialConsole,
}

#[derive(Debug)]
struct SerialConsole {
    #[debug("{vm_id}")]
    vm_id: VmId,
    #[debug("{}", path.display())]
    path: PathBuf,
    #[debug("{}", if stdin.is_some() { "open" } else { "closed" })]
    stdin: Option<FirecrackerStdin>,
    #[debug(
        "{}",
        match stdout_task.as_ref() {
            Some(task) if task.is_finished() => "finished",
            Some(_) => "running",
            None => "released",
        }
    )]
    stdout_task: Option<JoinHandle<()>>,
    #[debug(
        "{}",
        match stderr_task.as_ref() {
            Some(task) if task.is_finished() => "finished",
            Some(_) => "running",
            None => "released",
        }
    )]
    stderr_task: Option<JoinHandle<()>>,
}

impl SerialConsole {
    #[tracing::instrument(
        name = "attach_serial_console",
        skip(vm, path),
        fields(%vm_id, path = %path.display()),
        err
    )]
    async fn new(vm: &mut FirecrackerVm, vm_id: VmId, path: PathBuf) -> Result<Self> {
        let file = io_error!(
            HealthError,
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await,
            path.clone()
        )?;
        let pipes = vm.take_pipes()?;
        let console = Self {
            vm_id,
            path: path.clone(),
            stdin: Some(pipes.stdin),
            stdout_task: Some(Self::spawn_stdout_reader(pipes.stdout, file, vm_id, &path)),
            stderr_task: Some(Self::spawn_stderr_reader(pipes.stderr, vm_id)),
        };
        tracing::debug!(
            ?console,
            "barbirolli attached the Firecracker serial console"
        );
        Ok(console)
    }

    #[tracing::instrument(
        name = "write_serial_console",
        skip(self, bytes),
        fields(vm_id = %self.vm_id, path = %self.path.display(), byte_count = bytes.len()),
        err
    )]
    async fn write(&mut self, bytes: &[u8]) -> std::result::Result<(), SerialShutdownError> {
        tracing::debug!("barbirolli writes to the Firecracker serial input");
        let stdin = self
            .stdin
            .as_mut()
            .ok_or(SerialShutdownError::MissingStdin)?;
        stdin
            .write_all(bytes)
            .await
            .map_err(SerialShutdownError::Write)?;
        stdin.flush().await.map_err(SerialShutdownError::Write)
    }

    #[tracing::instrument(
        name = "finish_serial_console",
        skip(self),
        fields(vm_id = %self.vm_id, path = %self.path.display())
    )]
    async fn finish(&mut self) {
        drop(self.stdin.take());
        Self::join_task(&mut self.stdout_task, "stdout").await;
        Self::join_task(&mut self.stderr_task, "stderr").await;
        tracing::debug!(console = ?self, "the Firecracker serial console stopped");
    }

    #[tracing::instrument(
        name = "abort_serial_console",
        skip(self),
        fields(vm_id = %self.vm_id, path = %self.path.display())
    )]
    fn abort(&mut self) {
        drop(self.stdin.take());
        if let Some(task) = self.stdout_task.take() {
            task.abort();
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        tracing::warn!(
            console = ?self,
            "barbirolli stopped the Firecracker serial-console readers"
        );
    }
}

#[derive(Debug, thiserror::Error)]
enum SerialShutdownError {
    #[error("Firecracker serial stdin is unavailable")]
    MissingStdin,
    #[error("failed to write Firecracker serial stdin: {0}")]
    Write(#[source] std::io::Error),
    #[error("Firecracker did not exit before the serial shutdown timeout")]
    Timeout,
}

impl SerialConsole {
    fn spawn_stdout_reader(
        mut stdout: FirecrackerStdout,
        mut file: tokio::fs::File,
        vm_id: VmId,
        path: &Path,
    ) -> JoinHandle<()> {
        let span = info_span!("serial_reader", %vm_id, stream = "stdout", path = %path.display());
        tokio::spawn(
        async move {
            let mut buffer = vec![0_u8; 8 * 1024];
            let mut recording = true;
            loop {
                let read = match stdout.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(error) => {
                        tracing::error!(%error, "the Firecracker stdout reader failed");
                        break;
                    }
                };
                if recording
                    && let Err(error) =
                        tokio::io::AsyncWriteExt::write_all(&mut file, &buffer[..read]).await
                {
                    tracing::error!(
                        %error,
                        "the stdout reader continues because the Firecracker serial-log write failed"
                    );
                    recording = false;
                }
            }
            if recording && let Err(error) = tokio::io::AsyncWriteExt::flush(&mut file).await {
                tracing::error!(%error, "the Firecracker serial-log flush failed");
            }
        }
        .instrument(span),
    )
    }

    fn spawn_stderr_reader(mut stderr: FirecrackerStderr, vm_id: VmId) -> JoinHandle<()> {
        let span = info_span!("serial_reader", %vm_id, stream = "stderr");
        tokio::spawn(
            async move {
                let mut buffer = vec![0_u8; 8 * 1024];
                loop {
                    match stderr.read(&mut buffer).await {
                        Ok(0) => break,
                        Ok(read) => tracing::warn!(
                            byte_count = read,
                            stderr = ?String::from_utf8_lossy(&buffer[..read]),
                            "the Firecracker process wrote to stderr"
                        ),
                        Err(error) => {
                            tracing::error!(%error, "the Firecracker stderr reader failed");
                            break;
                        }
                    }
                }
            }
            .instrument(span),
        )
    }

    async fn join_task(task: &mut Option<JoinHandle<()>>, stream: &'static str) {
        if let Some(task) = task.take()
            && let Err(error) = task.await
        {
            tracing::error!(%error, stream, "the Firecracker serial-reader task failed");
        }
    }
}

impl FirecrackerExecutorConfig {
    fn ownership_model(&self, vm_id: VmId) -> VmmOwnershipModel {
        self.identity(vm_id)
            .map_or(VmmOwnershipModel::Shared, |identity| {
                VmmOwnershipModel::Downgraded {
                    uid: identity.uid,
                    gid: identity.gid,
                }
            })
    }

    fn executor(
        &self,
        arguments: VmmArguments,
        id: VmmId,
        vm_id: VmId,
    ) -> EitherVmmExecutor<FlatVirtualPathResolver> {
        match self {
            Self::Unrestricted => {
                EitherVmmExecutor::from(UnrestrictedVmmExecutor::new(arguments).id(id))
            }
            Self::Jailed(config) => {
                let jailer_arguments = JailerArguments::new(id)
                    .cgroup_version(JailerCgroupVersion::V2)
                    .parent_cgroup(VmCgroup::relative_path(vm_id))
                    .chroot_base_dir(&config.chroot_base);
                EitherVmmExecutor::from(JailedVmmExecutor::new(
                    arguments,
                    jailer_arguments,
                    FlatVirtualPathResolver,
                ))
            }
        }
    }

    fn launcher(&self, firecracker: &Path) -> PathBuf {
        match self {
            Self::Jailed(config) => config.jailer.clone(),
            Self::Unrestricted => firecracker.to_owned(),
        }
    }

    fn is_unrestricted(&self) -> bool {
        matches!(self, Self::Unrestricted)
    }
}

impl VmSpec {
    async fn prepare_vm(&self, barbirolli: &Barbirolli) -> Result<FirecrackerVm> {
        let mut resources = FirecrackerResourceSystem::with_capacity(
            DirectProcessSpawner,
            TokioRuntime,
            barbirolli.firecracker.executor.ownership_model(self.id),
            2,
        );
        let configuration = self.configuration(&mut resources, barbirolli.balloon.mem)?;
        let arguments = VmmArguments::new(VmmApiSocket::Enabled(
            barbirolli.firecracker.vmm_api_socket(self),
        ));
        let id = VmmId::new(format!("barbirolli-{}", self.id))?;
        let executor = barbirolli
            .firecracker
            .executor
            .executor(arguments, id, self.id);
        let installation = VmmInstallation::new(
            barbirolli.firecracker.bin.clone(),
            barbirolli
                .firecracker
                .executor
                .launcher(&barbirolli.firecracker.bin),
            barbirolli.firecracker.bin.clone(),
        );

        Ok(FirecrackerVm::prepare(executor, resources, installation, configuration).await?)
    }

    #[tracing::instrument(skip(self, barbirolli))]
    pub(crate) fn reconcile<'a>(&'a self, barbirolli: &'a Barbirolli) -> BoxFuture<'a, Result<()>> {
        let vm_id = self.id;
        async move {
            let mut errors = vec![];
            if barbirolli.firecracker.executor.is_unrestricted()
                && let Err(err) = self.kill_stale_processes(barbirolli).await
            {
                errors.push(ReconcileFailure::Processes(err));
            }
            if let Err(err) = self.network.cleanup_stale().await {
                errors.push(ReconcileFailure::Network(err));
            }
            if let Err(err) = self.cleanup_cgroup_and_metrics(&barbirolli.firecracker) {
                errors.push(ReconcileFailure::Health(err));
            }
            if let Err(err) = self.cleanup_api_sockets(&barbirolli.firecracker) {
                errors.push(ReconcileFailure::Socket(err));
            }
            if let Err(err) = barbirolli.firecracker.cleanup_jail(self) {
                errors.push(ReconcileFailure::Jail(err));
            }
            if let Err(err) = self.ensure_serial_log() {
                errors.push(ReconcileFailure::SerialLog(err));
            }
            match NEVec::try_from_vec(errors) {
                Some(failures) => Err(LifecycleError::Reconcile(Validated::Fail(failures))),
                None => Ok(()),
            }
        }
        .instrument(info_span!("reconcile_vm", %vm_id, operation = "reconcile"))
        .boxed()
    }

    #[tracing::instrument]
    async fn kill_stale_processes(&self, barbirolli: &Barbirolli) -> Result<(), std::io::Error> {
        match barbirolli.firecracker.pid(self).await {
            Ok(pid) => {
                rustix::process::kill_process(pid, rustix::process::Signal::KILL)?;
                Ok(())
            }
            Err(PidError::ProcessNotFound) => Ok(()),
            Err(PidError::Io { source, .. }) => Err(source),
        }
    }

    fn metrics_path(&self) -> PathBuf {
        self.artifact_dir.join("metrics.fifo")
    }

    pub(crate) fn ensure_serial_log(&self) -> Result<(), HealthError> {
        let path = self.serial_log();
        io_error!(
            HealthError,
            fs::OpenOptions::new().create(true).append(true).open(&path),
            path
        )?;
        Ok(())
    }

    fn cleanup_api_sockets(
        &self,
        firecracker: &crate::lifecycle::Firecracker,
    ) -> std::io::Result<()> {
        let logical: PathBuf = self.api_socket.clone().into();
        let effective = firecracker.effective_api_socket(self);
        self.api_socket.remove()?;
        if effective != logical {
            crate::lifecycle::Firecracker::remove_file_if_present(&effective)?;
        }
        Ok(())
    }

    #[tracing::instrument(err)]
    fn cleanup_cgroup_and_metrics(
        &self,
        firecracker: &crate::lifecycle::Firecracker,
    ) -> Result<(), HealthError> {
        tracing::info!("barbirolli starts cgroup and metrics cleanup");
        VmCgroup::from_id(self.id).cleanup()?;

        let metrics = self.metrics_path();
        let effective_metrics = firecracker.effective_path(self, &metrics);
        io_error!(
            HealthError,
            crate::lifecycle::Firecracker::remove_file_if_present(&metrics),
            metrics.clone()
        )?;
        if effective_metrics != metrics {
            io_error!(
                HealthError,
                crate::lifecycle::Firecracker::remove_file_if_present(&effective_metrics),
                effective_metrics
            )?;
        }

        Ok(())
    }
}

impl VmSpec {
    fn configuration(
        &self,
        resources: &mut FirecrackerResourceSystem,
        balloon_mem: crate::MemoryMib,
    ) -> Result<VmConfiguration> {
        let kernel = resources.create_resource(
            &self.kernel,
            ResourceType::Moved(MovedResourceType::HardLinkedOrCopied),
        )?;
        let rootfs = resources.create_resource(
            self.rootfs.as_ref(),
            ResourceType::Moved(MovedResourceType::HardLinkedOrCopied),
        )?;
        let metrics = resources.create_resource(
            self.metrics_path(),
            ResourceType::Created(CreatedResourceType::Fifo),
        )?;

        Ok(VmConfiguration::New {
            init_method: InitMethod::ViaApiCalls,
            data: VmConfigurationData {
                boot_source: BootSource {
                    kernel_image: kernel,
                    boot_args: Some(self.network.kernel_boot_args()),
                    initrd: None,
                },
                drives: vec![Drive {
                    drive_id: "rootfs".into(),
                    is_root_device: true,
                    cache_type: None,
                    partuuid: None,
                    is_read_only: Some(false),
                    block: Some(rootfs),
                    rate_limiter: None,
                    io_engine: None,
                    socket: None,
                }],
                machine_configuration: MachineConfiguration {
                    vcpu_count: self.vcpu_count.into(),
                    mem_size_mib: u16::from(self.memory_mib).into(),
                    smt: None,
                    track_dirty_pages: None,
                    huge_pages: None,
                },
                cpu_template: None,
                network_interfaces: vec![NetworkInterface {
                    iface_id: "eth0".into(),
                    host_dev_name: self.network.tap.as_ref().to_owned(),
                    guest_mac: Some(self.network.guest_mac.clone()),
                    rx_rate_limiter: None,
                    tx_rate_limiter: None,
                }],
                balloon_device: Some(BalloonDevice {
                    amount_mib: i32::from(u16::from(balloon_mem)),
                    deflate_on_oom: true,
                    stats_polling_interval_s: Some(10),
                }),
                vsock_device: None,
                logger_system: None,
                metrics_system: Some(MetricsSystem { metrics }),
                mmds_configuration: None,
                entropy_device: None,
            },
        })
    }
}

impl ManagedVm {
    async fn rollback_vm(vm: &mut FirecrackerVm, shutdown_timeout: Duration) -> Result<()> {
        let shutdown = match vm.get_state() {
            VmState::NotStarted | VmState::Exited | VmState::Crashed(_) => Ok(()),
            VmState::Running | VmState::Paused => vm
                .shutdown([VmShutdownAction {
                    method: VmShutdownMethod::Kill,
                    timeout: Some(shutdown_timeout),
                    graceful: false,
                }])
                .await
                .map(|_| ())
                .map_err(LifecycleError::from),
        };
        let cleanup = vm.cleanup().await.map_err(LifecycleError::from);
        match (shutdown, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(shutdown), Err(cleanup)) => Err(LifecycleError::ShutdownCleanup {
                shutdown: Some(Box::new(shutdown)),
                cleanup: Some(Box::new(cleanup)),
            }),
        }
    }

    async fn rollback_vm_with_console(
        vm: &mut FirecrackerVm,
        console: &mut SerialConsole,
        shutdown_timeout: Duration,
    ) -> Result<()> {
        let rollback = Self::rollback_vm(vm, shutdown_timeout).await;
        if matches!(vm.get_state(), VmState::Exited | VmState::Crashed(_)) {
            console.finish().await;
        } else {
            console.abort();
        }
        rollback
    }

    async fn rollback_startup(
        startup_error: LifecycleError,
        vm: Option<&mut FirecrackerVm>,
        console: Option<&mut SerialConsole>,
        network: ManagedNetwork,
        cgroup: Option<VmCgroup>,
        spec: &VmSpec,
        barbirolli: &Barbirolli,
    ) -> LifecycleError {
        let vm_rollback = match (vm, console) {
            (Some(vm), Some(console)) => {
                Self::rollback_vm_with_console(vm, console, barbirolli.shutdown_timeout).await
            }
            (Some(vm), None) => Self::rollback_vm(vm, barbirolli.shutdown_timeout).await,
            (None, _) => Ok(()),
        };
        let had_cgroup = cgroup.is_some();
        let cgroup_rollback = cgroup.map_or(Ok(()), VmCgroup::cleanup);
        let jail_rollback = if had_cgroup && cgroup_rollback.is_ok() {
            barbirolli.firecracker.cleanup_jail(spec)
        } else {
            Ok(())
        };
        let network_rollback = network.cleanup().await;

        LifecycleError::StartupRollback {
            startup_error: Box::new(startup_error),
            vm_rollback_error: vm_rollback.err().map(Box::new),
            network_rollback_error: network_rollback.err().map(Box::new),
            cgroup_rollback_error: cgroup_rollback.err().map(Box::new),
            jail_rollback_error: jail_rollback.err().map(Box::new),
        }
    }
}

impl ManagedVm {
    #[tracing::instrument(skip(spec, barbirolli), fields(vm_id = %spec.id), err)]
    pub async fn start(spec: VmSpec, barbirolli: &Barbirolli) -> Result<Self> {
        let identity = barbirolli.firecracker.executor.identity(spec.id);
        let network = spec
            .network
            .prepare(&spec.bindings, identity.map(|identity| identity.uid))
            .await?;
        let cgroup = match VmCgroup::create(spec.id) {
            Ok(cgroup) => cgroup,
            Err(startup) => {
                return Err(Self::rollback_startup(
                    LifecycleError::Health(startup),
                    None,
                    None,
                    network,
                    None,
                    &spec,
                    barbirolli,
                )
                .await);
            }
        };
        let mut vm = match spec.prepare_vm(barbirolli).await {
            Ok(vm) => vm,
            Err(startup) => {
                return Err(Self::rollback_startup(
                    startup,
                    None,
                    None,
                    network,
                    Some(cgroup),
                    &spec,
                    barbirolli,
                )
                .await);
            }
        };
        let metrics_path = vm.resolve_effective_path(spec.metrics_path());

        if let Err(error) = vm.start(barbirolli.firecracker.api_socket_timeout).await {
            return Err(Self::rollback_startup(
                error.into(),
                Some(&mut vm),
                None,
                network,
                Some(cgroup),
                &spec,
                barbirolli,
            )
            .await);
        }

        let mut console = match SerialConsole::new(&mut vm, spec.id, spec.serial_log()).await {
            Ok(serial_console) => serial_console,
            Err(startup) => {
                return Err(Self::rollback_startup(
                    startup,
                    Some(&mut vm),
                    None,
                    network,
                    Some(cgroup),
                    &spec,
                    barbirolli,
                )
                .await);
            }
        };

        let pid = cgroup.pid_for(&barbirolli.firecracker, &spec).await;
        let pid = match pid {
            Ok(pid) => pid,
            Err(error) => {
                return Err(Self::rollback_startup(
                    LifecycleError::Health(error),
                    Some(&mut vm),
                    Some(&mut console),
                    network,
                    Some(cgroup),
                    &spec,
                    barbirolli,
                )
                .await);
            }
        };
        let metrics = Arc::new(RwLock::new(None));

        let managed = Self {
            vm,
            network: Some(network),
            failed: false,
            pid,
            monitor: Mutex::new(None),
            cgroup: Some(cgroup),
            metrics_task: MetricsReader::spawn(metrics_path, metrics.clone()),
            metrics,
            console,
            spec,
        };
        tracing::info!(
            pid = managed.pid.as_raw_pid(),
            "barbirolli started the managed VM"
        );
        Ok(managed)
    }

    pub async fn balloon_config(&mut self) -> Result<BalloonDevice> {
        Ok(self.vm.get_balloon_device().await?)
    }

    pub async fn update_balloon(&mut self, amount_mib: u16) -> Result<()> {
        Ok(self
            .vm
            .update_balloon_device(UpdateBalloonDevice { amount_mib })
            .await?)
    }

    pub async fn balloon_statistics(&mut self) -> Result<BalloonStatistics> {
        Ok(self.vm.get_balloon_statistics().await?)
    }

    pub async fn shutdown(&mut self, timeout: Duration) -> Result<()> {
        if matches!(self.vm.get_state(), VmState::Exited | VmState::Crashed(_)) {
            return Ok(());
        }

        if cfg!(target_arch = "x86_64") {
            self.vm
                .shutdown([
                    VmShutdownAction {
                        method: VmShutdownMethod::CtrlAltDel,
                        timeout: Some(timeout),
                        graceful: true,
                    },
                    VmShutdownAction {
                        method: VmShutdownMethod::PauseThenKill,
                        timeout: Some(timeout / 2),
                        graceful: false,
                    },
                    VmShutdownAction {
                        method: VmShutdownMethod::Kill,
                        timeout: Some(timeout / 2),
                        graceful: false,
                    },
                ])
                .await?;
            return Ok(());
        }

        if let Err(error) = self.shutdown_through_serial(timeout).await {
            tracing::warn!(%error, vm_id = %self.spec.id, "the graceful serial shutdown failed");
        }
        if matches!(self.vm.get_state(), VmState::Exited | VmState::Crashed(_)) {
            return Ok(());
        }
        self.vm
            .shutdown([
                VmShutdownAction {
                    method: VmShutdownMethod::PauseThenKill,
                    timeout: Some(timeout / 2),
                    graceful: false,
                },
                VmShutdownAction {
                    method: VmShutdownMethod::Kill,
                    timeout: Some(timeout / 2),
                    graceful: false,
                },
            ])
            .await?;
        Ok(())
    }

    async fn shutdown_through_serial(
        &mut self,
        duration: Duration,
    ) -> Result<(), SerialShutdownError> {
        self.console.write(b"reboot\n").await?;
        timeout(duration, async {
            loop {
                if matches!(self.vm.get_state(), VmState::Exited | VmState::Crashed(_)) {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| SerialShutdownError::Timeout)
    }

    #[tracing::instrument(skip(self), fields(vm_id = %self.spec.id), err)]
    pub async fn cleanup(&mut self) -> Result<()> {
        tracing::info!("barbirolli starts managed VM cleanup");
        self.metrics_task.abort();
        if matches!(self.vm.get_state(), VmState::Exited | VmState::Crashed(_)) {
            self.console.finish().await;
        } else {
            self.console.abort();
        }
        let vm = self.vm.cleanup().await.map_err(LifecycleError::from);
        let network = match self.network.as_ref() {
            Some(network) => network.cleanup().await,
            None => Ok(()),
        };
        if network.is_ok() {
            self.network = None;
        }

        let cgroup = self
            .cgroup
            .take()
            .map_or(Ok(()), VmCgroup::cleanup)
            .map_err(LifecycleError::Health);

        match (vm, network, cgroup) {
            (Ok(()), Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(()), Ok(())) | (Ok(()), Ok(()), Err(error)) => Err(error),
            (Ok(()), Err(error), Ok(())) => Err(error.into()),
            (vm, network, cgroup) => Err(LifecycleError::Cleanup {
                vm: vm.err().map(Box::new),
                network: network.err().map(Box::new),
                health: cgroup.err().map(Box::new),
            }),
        }
    }

    pub(crate) fn activity_sample(&self) -> Result<Sample, HealthError> {
        let cgroup = self.cgroup.as_ref().ok_or(HealthError::MissingCgroup)?;
        let metrics = self
            .metrics
            .read()
            .map_err(|_| HealthError::MetricsLock)?
            .clone()
            .ok_or(HealthError::MissingMetrics)?;
        let connections = self.connection_counts();
        Ok(Sample {
            instance_pid: self.pid.as_raw_pid(),
            sampled_at: Instant::now(),
            cpu_usage_usec: cgroup.read_key("cpu.stat", "usage_usec")?,
            memory_bytes: cgroup.read_value("memory.current")?,
            process_count: cgroup.read_value("pids.current")?,
            network_bytes: metrics
                .net
                .rx_bytes_count
                .saturating_add(metrics.net.tx_bytes_count),
            disk_bytes: metrics
                .block
                .read_bytes
                .saturating_add(metrics.block.write_bytes),
            established_tcp_connections: connections.1,
            total_connections: connections.0,
            vcpu_count: self.spec.vcpu_count.into(),
        })
    }
}

struct MetricsReader;

impl MetricsReader {
    fn spawn(path: PathBuf, latest: Arc<RwLock<Option<Metrics>>>) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(error) = Self::read(&path, &latest).await {
                tracing::error!(%error, path = %path.display(), "the Firecracker metrics reader failed");
            }
        })
    }

    async fn read(path: &Path, latest: &RwLock<Option<Metrics>>) -> Result<(), HealthError> {
        let file = io_error!(
            HealthError,
            tokio::fs::File::open(path).await,
            path.to_owned()
        )?;
        let mut lines = BufReader::new(file).lines();
        while let Some(line) = io_error!(HealthError, lines.next_line().await, path.to_owned())? {
            let metrics = match serde_json::from_str(&line) {
                Ok(metrics) => metrics,
                Err(error) => {
                    tracing::warn!(%error, "barbirolli discarded invalid Firecracker metrics");
                    continue;
                }
            };
            *latest.write().map_err(|_| HealthError::MetricsLock)? = Some(metrics);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct VmCgroup {
    path: PathBuf,
}

impl VmCgroup {
    const ROOT: &'static str = "/sys/fs/cgroup/barbirolli";

    fn path(id: VmId) -> PathBuf {
        PathBuf::from(Self::ROOT).join(format!("vm-{id}"))
    }

    fn from_id(id: VmId) -> Self {
        Self {
            path: Self::path(id),
        }
    }

    fn relative_path(id: VmId) -> String {
        format!("barbirolli/vm-{id}")
    }

    fn create(id: VmId) -> Result<Self, HealthError> {
        let root = PathBuf::from(Self::ROOT);
        io_error!(HealthError, fs::create_dir_all(&root), root.clone())?;
        let controllers = root.join("cgroup.subtree_control");
        io_error!(
            HealthError,
            fs::write(&controllers, "+cpu +memory +pids"),
            controllers
        )?;
        let path = Self::path(id);
        io_error!(HealthError, fs::create_dir(&path), path.clone())?;
        Ok(Self { path })
    }

    fn attach(&self, pid: Pid) -> Result<(), HealthError> {
        let procs = self.path.join("cgroup.procs");
        io_error!(
            HealthError,
            fs::write(&procs, pid.as_raw_pid().to_string()),
            procs
        )
    }

    fn pid(&self) -> Result<Pid, HealthError> {
        let procs = self.path.join("cgroup.procs");
        let contents = io_error!(HealthError, fs::read_to_string(&procs), procs.clone())?;
        contents
            .lines()
            .find_map(|value| value.parse::<i32>().ok().and_then(Pid::from_raw))
            .ok_or(HealthError::InvalidValue(procs))
    }

    async fn pid_for(
        &self,
        firecracker: &crate::lifecycle::Firecracker,
        spec: &VmSpec,
    ) -> Result<Pid, HealthError> {
        if !firecracker.executor.is_unrestricted() {
            return self.pid();
        }
        let pid = firecracker.pid(spec).await.map_err(HealthError::from)?;
        self.attach(pid).map(|()| pid)
    }

    fn read_value(&self, name: &str) -> Result<u64, HealthError> {
        let path = self.path.join(name);
        let value = io_error!(HealthError, fs::read_to_string(&path), path.clone())?;
        value
            .trim()
            .parse()
            .map_err(|_| HealthError::InvalidValue(path))
    }

    fn read_key(&self, name: &str, key: &str) -> Result<u64, HealthError> {
        let path = self.path.join(name);
        let value = io_error!(HealthError, fs::read_to_string(&path), path.clone())?;
        value
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(' ')?;
                (name == key).then(|| value.parse().ok()).flatten()
            })
            .ok_or(HealthError::InvalidValue(path))
    }

    fn cleanup(self) -> Result<(), HealthError> {
        let kill = self.path.join("cgroup.kill");
        if kill.exists() {
            io_error!(HealthError, fs::write(&kill, "1"), kill)?;
        }
        let cleanup = match fs::remove_dir(&self.path) {
            Err(source) if source.kind() == ErrorKind::NotFound => Ok(()),
            result => result,
        };
        io_error!(HealthError, cleanup, self.path)
    }
}

impl ManagedVm {
    fn connection_counts(&self) -> (u64, u64) {
        let guest_ip = self.spec.network.guest_ip;
        let Some(contents) = ["/proc/net/nf_conntrack", "/proc/net/ip_conntrack"]
            .into_iter()
            .find_map(|path| fs::read_to_string(path).ok())
        else {
            return (0, 0);
        };
        let address = guest_ip.to_string();
        contents
            .lines()
            .filter(|line| {
                line.split_ascii_whitespace().any(|field| {
                    field
                        .strip_prefix("src=")
                        .or_else(|| field.strip_prefix("dst="))
                        .is_some_and(|ip| ip == address)
                })
            })
            .fold((0, 0), |(total, established), line| {
                (
                    total + 1,
                    established
                        + u64::from(
                            line.split_ascii_whitespace()
                                .any(|value| value == "ESTABLISHED"),
                        ),
                )
            })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error(transparent)]
    Network(#[from] NetworkError),
    #[error(transparent)]
    ResourceSystem(#[from] ResourceSystemError),
    #[error(transparent)]
    VmmId(#[from] VmmIdError),
    #[error(transparent)]
    Vm(#[from] FctoolsVmError),
    #[error(transparent)]
    VmApi(#[from] VmApiError),
    #[error(transparent)]
    Shutdown(#[from] VmShutdownError),
    #[error(
        "startup failed: {startup_error}; VM rollback: {vm_rollback_error:?}; network rollback: {network_rollback_error:?}; cgroup rollback: {cgroup_rollback_error:?}; jail rollback: {jail_rollback_error:?}"
    )]
    StartupRollback {
        startup_error: Box<LifecycleError>,
        vm_rollback_error: Option<Box<LifecycleError>>,
        network_rollback_error: Option<Box<NetworkError>>,
        cgroup_rollback_error: Option<Box<HealthError>>,
        jail_rollback_error: Option<Box<std::io::Error>>,
    },
    #[error("VM cleanup failed (VM: {vm:?}, network: {network:?}, health: {health:?})")]
    Cleanup {
        vm: Option<Box<LifecycleError>>,
        network: Option<Box<NetworkError>>,
        health: Option<Box<LifecycleError>>,
    },
    #[error("VM shutdown/cleanup failed (shutdown: {shutdown:?}, cleanup: {cleanup:?})")]
    ShutdownCleanup {
        shutdown: Option<Box<LifecycleError>>,
        cleanup: Option<Box<LifecycleError>>,
    },
    #[error("warmup reconciliation failed: {0:?}")]
    Reconcile(Validated<(), ReconcileFailure>),
    #[error(transparent)]
    Health(#[from] HealthError),
    #[error("FIRECRACKER must name a Firecracker 1.13 executable, got {0:?}")]
    UnsupportedFirecracker(String),
    #[error("JAILER must name a jailer executable matching Firecracker, got {0:?}")]
    UnsupportedJailer(String),
    #[error(
        "Firecracker and jailer versions must match exactly (Firecracker {firecracker}, jailer {jailer})"
    )]
    MismatchedJailerVersion { firecracker: String, jailer: String },
    #[error("untrusted jailer toolchain path {path}: {reason}")]
    UntrustedJailerPath { path: PathBuf, reason: String },
    #[error("the effective jailed API socket path exceeds Linux's Unix-socket limit: {0}")]
    JailerSocketPathTooLong(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum HealthError {
    #[error("failed to access health resource {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid health value in {0}")]
    InvalidValue(PathBuf),
    #[error("pid error: {0}")]
    PidError(#[from] PidError),
    #[error("the VM cgroup is unavailable")]
    MissingCgroup,
    #[error("Firecracker has not emitted a metrics sample")]
    MissingMetrics,
    #[error("the Firecracker metrics lock was poisoned")]
    MetricsLock,
    #[error("the VM monitor lock was poisoned")]
    MonitorLock,
}

impl crate::IoError for HealthError {
    fn from_io_error(path: PathBuf, source: std::io::Error) -> Self {
        Self::Io { path, source }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReconcileFailure {
    #[error("failed to kill stale Firecracker processes")]
    Processes(#[source] std::io::Error),
    #[error("failed to clean up stale network")]
    Network(#[source] NetworkError),
    #[error("failed to remove stale Firecracker API socket")]
    Socket(#[source] std::io::Error),
    #[error("failed to clean up stale health resources")]
    Health(#[source] HealthError),
    #[error("failed to prepare the VM serial log")]
    SerialLog(#[source] HealthError),
    #[error("failed to remove the stale Firecracker jail")]
    Jail(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::Path};

    use super::*;

    fn shell(script: &str) -> FirecrackerChild {
        TokioRuntime
            .spawn_process(
                Path::new("/bin/sh").as_os_str(),
                &[OsString::from("-c"), OsString::from(script)],
                true,
                true,
                true,
            )
            .expect("failed to spawn test process")
    }

    #[tokio::test]
    async fn serial_reader_appends_ordered_stdout_and_excludes_stderr() {
        let temporary = tempfile::tempdir().expect("failed to create temporary directory");
        let path = temporary.path().join("serial.log");
        tokio::fs::write(&path, b"previous boot\n")
            .await
            .expect("failed to seed serial log");
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .expect("failed to open serial log");
        let mut child =
            shell("printf 'first line\\n'; printf 'stderr only\\n' >&2; printf 'second line\\n'");
        let stdout = child.take_stdout().expect("missing stdout pipe");
        let stderr = child.take_stderr().expect("missing stderr pipe");
        let vm_id = VmId::try_from(0).expect("valid test VM ID");
        let stdout_task = SerialConsole::spawn_stdout_reader(stdout, file, vm_id, &path);
        let stderr_task = SerialConsole::spawn_stderr_reader(stderr, vm_id);

        assert!(
            child
                .wait()
                .await
                .expect("test process wait failed")
                .success()
        );
        stdout_task.await.expect("stdout reader task failed");
        stderr_task.await.expect("stderr reader task failed");

        assert_eq!(
            tokio::fs::read(&path)
                .await
                .expect("failed to read serial log"),
            b"previous boot\nfirst line\nsecond line\n"
        );
    }

    #[tokio::test]
    async fn serial_console_retains_stdin_for_shutdown_writes() {
        let temporary = tempfile::tempdir().expect("failed to create temporary directory");
        let path = temporary.path().join("serial.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .expect("failed to open serial log");
        let mut child = shell("IFS= read -r line; printf '%s\\n' \"$line\"");
        let vm_id = VmId::try_from(0).expect("valid test VM ID");
        let mut console = SerialConsole {
            vm_id,
            path: path.clone(),
            stdin: child.take_stdin(),
            stdout_task: Some(SerialConsole::spawn_stdout_reader(
                child.take_stdout().expect("missing stdout pipe"),
                file,
                vm_id,
                &path,
            )),
            stderr_task: Some(SerialConsole::spawn_stderr_reader(
                child.take_stderr().expect("missing stderr pipe"),
                vm_id,
            )),
        };

        let active = format!("{console:?}");
        assert!(active.contains("vm_id: 0"));
        assert!(active.contains(&format!("path: {}", path.display())));
        assert!(active.contains("stdin: open"));
        assert!(active.contains("stdout_task: running"));
        assert!(active.contains("stderr_task: running"));

        console
            .write(b"reboot\n")
            .await
            .expect("failed to write retained serial stdin");
        assert!(
            child
                .wait()
                .await
                .expect("test process wait failed")
                .success()
        );
        console.finish().await;

        let finished = format!("{console:?}");
        assert!(finished.contains("stdin: closed"));
        assert!(finished.contains("stdout_task: released"));
        assert!(finished.contains("stderr_task: released"));

        assert_eq!(
            tokio::fs::read(&path)
                .await
                .expect("failed to read serial log"),
            b"reboot\n"
        );
    }
}
