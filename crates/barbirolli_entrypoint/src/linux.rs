use std::{
    env, fs, io,
    os::unix::process::CommandExt as _,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
};

use nix::{
    errno::Errno,
    mount::{MsFlags, mount},
    sys::{
        prctl::set_no_new_privs,
        reboot::{RebootMode, reboot},
        resource::{Resource, setrlimit},
        signal::{SigSet, SigmaskHow, Signal, killpg},
        stat::{Mode, umask},
    },
    unistd::{Gid, Pid, Uid, chdir, setgroups, setpgid, setresgid, setresuid, sync},
};
use oci_spec::runtime::PosixRlimitType;

use crate::{
    EntrypointError, ProcessSpec, RUN_PROCESS_SIGNAL_MASK_ENV, RUN_PROCESS_SPEC_ENV, Result,
};

const SUPERVISED_SIGNALS: [Signal; 7] = [
    Signal::SIGCHLD,
    Signal::SIGTERM,
    Signal::SIGINT,
    Signal::SIGHUP,
    Signal::SIGQUIT,
    Signal::SIGUSR1,
    Signal::SIGUSR2,
];
const SUPERVISED_SIGNAL_BITS: u8 = 0b0111_1111;

macro_rules! configure {
    ($operation:literal, $result:expr $(,)?) => {
        $result.map_err(|source| EntrypointError::ConfigureProcess {
            operation: $operation,
            source,
        })
    };
}

struct MountSpec {
    source: &'static str,
    filesystem: &'static str,
    filesystem_name: &'static str,
    target: Target,
    flags: MsFlags,
    data: Option<&'static str>,
}

impl MountSpec {
    fn mount(&self) -> Result<()> {
        let target = Path::new(self.target.0);
        fs::create_dir_all(target).map_err(|source| EntrypointError::CreateMountpoint {
            path: target.to_owned(),
            source,
        })?;
        if self.target.is_mounted()? {
            return Ok(());
        }

        mount(
            Some(self.source),
            target,
            Some(self.filesystem),
            self.flags,
            self.data,
        )
        .map_err(|source| EntrypointError::MountFilesystem {
            filesystem: self.filesystem_name,
            target: self.target.0,
            source,
        })
    }

    const PROC: MountSpec = MountSpec {
        source: "proc",
        filesystem: "proc",
        filesystem_name: "proc",
        target: Target("/proc"),
        flags: MsFlags::MS_NOSUID
            .union(MsFlags::MS_NODEV)
            .union(MsFlags::MS_NOEXEC),
        data: None,
    };

    const USERSPACE: [MountSpec; 7] = [
        MountSpec::PROC,
        MountSpec {
            source: "sysfs",
            filesystem: "sysfs",
            filesystem_name: "sysfs",
            target: Target("/sys"),
            flags: MsFlags::MS_NOSUID
                .union(MsFlags::MS_NODEV)
                .union(MsFlags::MS_NOEXEC),
            data: None,
        },
        MountSpec {
            source: "devtmpfs",
            filesystem: "devtmpfs",
            filesystem_name: "devtmpfs",
            target: Target("/dev"),
            flags: MsFlags::MS_NOSUID,
            data: Some("mode=0755"),
        },
        MountSpec {
            source: "devpts",
            filesystem: "devpts",
            filesystem_name: "devpts",
            target: Target("/dev/pts"),
            flags: MsFlags::MS_NOSUID.union(MsFlags::MS_NOEXEC),
            data: Some("newinstance,ptmxmode=0666,mode=0620,gid=5"),
        },
        MountSpec {
            source: "tmpfs",
            filesystem: "tmpfs",
            filesystem_name: "tmpfs",
            target: Target("/dev/shm"),
            flags: MsFlags::MS_NOSUID.union(MsFlags::MS_NODEV),
            data: Some("mode=1777"),
        },
        MountSpec {
            source: "tmpfs",
            filesystem: "tmpfs",
            filesystem_name: "tmpfs",
            target: Target("/run"),
            flags: MsFlags::MS_NOSUID.union(MsFlags::MS_NODEV),
            data: Some("mode=0755"),
        },
        MountSpec {
            source: "cgroup2",
            filesystem: "cgroup2",
            filesystem_name: "cgroup2",
            target: Target("/sys/fs/cgroup"),
            flags: MsFlags::MS_NOSUID
                .union(MsFlags::MS_NODEV)
                .union(MsFlags::MS_NOEXEC),
            data: None,
        },
    ];
}

/// Mounts procfs so [`pid1`] can resolve the current executable before it
/// relaunches PID 1.
///
/// # Errors
///
/// Returns an error when `/proc` cannot be created, inspected, or mounted.
pub fn mount_proc_filesystem() -> Result<()> {
    MountSpec::PROC.mount()
}

/// Mounts the filesystems required by the VM userspace.
///
/// # Errors
///
/// Returns an error when the current mounts cannot be inspected, a mountpoint
/// cannot be created, or a required filesystem cannot be mounted.
pub fn mount_userspace_filesystems() -> Result<()> {
    for filesystem in &MountSpec::USERSPACE {
        filesystem.mount()?;
    }
    Ok(())
}

struct Target(&'static str);

impl Target {
    fn is_mounted(&self) -> Result<bool> {
        let mountinfo = match fs::read_to_string("/proc/self/mountinfo") {
            Ok(mountinfo) => mountinfo,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(EntrypointError::InspectMounts(error)),
        };
        Ok(mountinfo.lines().any(|line| {
            line.split_ascii_whitespace()
                .nth(4)
                .is_some_and(|mountpoint| mountpoint == self.0)
        }))
    }
}

pub struct OciChild {
    child: Child,
    signal_mask: SignalMask,
}

impl OciChild {
    fn restore_signal_mask(&mut self) -> Result<()> {
        self.signal_mask
            .restore()
            .map_err(EntrypointError::SignalSupervision)
    }

    /// Waits for the OCI process while forwarding signals to its process group.
    ///
    /// The outer [`pid1`] supervisor adopts and reaps orphaned descendants.
    ///
    /// # Errors
    ///
    /// Returns an error when a signal cannot be received or forwarded, the OCI
    /// process cannot be waited on, or the entrypoint signal mask cannot be
    /// restored.
    pub fn supervise_and_reap(mut self) -> Result<ExitStatus> {
        let process_id = self.child.id();
        let process_group = i32::try_from(process_id)
            .map(Pid::from_raw)
            .map_err(|_| EntrypointError::InvalidProcessId(process_id))?;
        let result = self.supervise(process_group);
        let restore = self.restore_signal_mask();

        match (result, restore) {
            (Ok(exit), Ok(())) => Ok(exit),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    fn supervise(&mut self, process_group: Pid) -> Result<ExitStatus> {
        loop {
            if let Some(exit) = self
                .child
                .try_wait()
                .map_err(EntrypointError::WaitForProcess)?
            {
                return Ok(exit);
            }

            let signal = self.signal_mask.wait()?;
            if signal == Signal::SIGCHLD {
                continue;
            }
            if let Err(source) = killpg(process_group, signal)
                && source != Errno::ESRCH
            {
                return Err(EntrypointError::ForwardSignal { signal, source });
            }
        }
    }
}

impl ProcessSpec {
    /// Spawns the configured OCI process through a clean entrypoint re-exec.
    ///
    /// The re-exec lets the child apply its `nix` process settings before the
    /// final `exec` without running Rust code in a `pre_exec` callback.
    ///
    /// # Errors
    ///
    /// Returns an error when signal supervision cannot be configured, the
    /// current entrypoint executable cannot be located, or the helper process
    /// cannot be spawned.
    pub fn spawn(self) -> Result<OciChild> {
        let source_path = self
            .source_path
            .as_ref()
            .ok_or(EntrypointError::InMemoryProcessSpec)?;
        let program = self.program.clone();
        let signal_mask = SignalMask::block()?;
        let entrypoint = env::current_exe().map_err(EntrypointError::CurrentExecutable)?;
        let child = Command::new(entrypoint)
            .env(RUN_PROCESS_SPEC_ENV, source_path)
            .env(
                RUN_PROCESS_SIGNAL_MASK_ENV,
                signal_mask.original_bits().to_string(),
            )
            .process_group(0)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|source| EntrypointError::SpawnProcess { program, source })?;

        Ok(OciChild { child, signal_mask })
    }

    fn exec(self, original_signal_mask: u8) -> Result<()> {
        let ProcessSpec {
            source_path: _,
            program,
            arguments,
            environment,
            working_directory,
            uid,
            gid,
            additional_gids,
            umask: process_umask,
            rlimits,
            no_new_privileges,
        } = self;
        let mut command = Command::new(&program);
        command
            .args(arguments)
            .env_clear()
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        for variable in environment {
            command.env(variable.name, variable.value);
        }

        restore_process_signal_mask(original_signal_mask)?;
        configure!("process group", setpgid(Pid::from_raw(0), Pid::from_raw(0)))?;
        configure!("working directory", chdir(&working_directory))?;
        for rlimit in rlimits {
            configure!(
                "resource limits",
                setrlimit(rlimit_resource(rlimit.resource), rlimit.soft, rlimit.hard),
            )?;
        }

        let additional_gids = additional_gids
            .into_iter()
            .map(Gid::from_raw)
            .collect::<Vec<_>>();
        configure!("supplementary groups", setgroups(&additional_gids))?;
        let gid = Gid::from_raw(gid);
        configure!("group IDs", setresgid(gid, gid, gid))?;
        let uid = Uid::from_raw(uid);
        configure!("user IDs", setresuid(uid, uid, uid))?;
        if no_new_privileges {
            configure!("no_new_privileges", set_no_new_privs())?;
        }
        if let Some(mask) = process_umask {
            let mask = Mode::from_bits(mask).ok_or(EntrypointError::InvalidProcessRunnerState)?;
            umask(mask);
        }

        let source = command.exec();
        Err(EntrypointError::SpawnProcess { program, source })
    }
}

/// Runs the OCI process when this entrypoint was relaunched as its safe process
/// configuration helper.
///
/// # Errors
///
/// Returns an error when the helper state is invalid, the process specification
/// cannot be loaded, process settings cannot be applied, or the final `exec`
/// fails.
pub fn run_oci_process_if_requested() -> Result<()> {
    let Some(spec_path) = env::var_os(RUN_PROCESS_SPEC_ENV) else {
        return Ok(());
    };
    let original_signal_mask = env::var(RUN_PROCESS_SIGNAL_MASK_ENV)
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|mask| mask & !SUPERVISED_SIGNAL_BITS == 0)
        .ok_or(EntrypointError::InvalidProcessRunnerState)?;

    ProcessSpec::new(spec_path)?.exec(original_signal_mask)
}

fn rlimit_resource(resource: PosixRlimitType) -> Resource {
    match resource {
        PosixRlimitType::RlimitCpu => Resource::RLIMIT_CPU,
        PosixRlimitType::RlimitFsize => Resource::RLIMIT_FSIZE,
        PosixRlimitType::RlimitData => Resource::RLIMIT_DATA,
        PosixRlimitType::RlimitStack => Resource::RLIMIT_STACK,
        PosixRlimitType::RlimitCore => Resource::RLIMIT_CORE,
        PosixRlimitType::RlimitRss => Resource::RLIMIT_RSS,
        PosixRlimitType::RlimitNproc => Resource::RLIMIT_NPROC,
        PosixRlimitType::RlimitNofile => Resource::RLIMIT_NOFILE,
        PosixRlimitType::RlimitMemlock => Resource::RLIMIT_MEMLOCK,
        PosixRlimitType::RlimitAs => Resource::RLIMIT_AS,
        PosixRlimitType::RlimitLocks => Resource::RLIMIT_LOCKS,
        PosixRlimitType::RlimitSigpending => Resource::RLIMIT_SIGPENDING,
        PosixRlimitType::RlimitMsgqueue => Resource::RLIMIT_MSGQUEUE,
        PosixRlimitType::RlimitNice => Resource::RLIMIT_NICE,
        PosixRlimitType::RlimitRtprio => Resource::RLIMIT_RTPRIO,
        PosixRlimitType::RlimitRttime => Resource::RLIMIT_RTTIME,
    }
}

struct SignalMask {
    blocked: SigSet,
    original: SigSet,
    active: bool,
}

impl SignalMask {
    fn block() -> Result<Self> {
        let blocked = supervised_signal_set();
        let original = blocked
            .thread_swap_mask(SigmaskHow::SIG_BLOCK)
            .map_err(EntrypointError::SignalSupervision)?;
        Ok(Self {
            blocked,
            original,
            active: true,
        })
    }

    fn original_bits(&self) -> u8 {
        SUPERVISED_SIGNALS
            .iter()
            .enumerate()
            .fold(0, |bits, (index, signal)| {
                if self.original.contains(*signal) {
                    bits | (1 << index)
                } else {
                    bits
                }
            })
    }

    fn wait(&self) -> Result<Signal> {
        self.blocked
            .wait()
            .map_err(EntrypointError::SignalSupervision)
    }

    fn restore(&mut self) -> nix::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.original.thread_set_mask()?;
        self.active = false;
        Ok(())
    }
}

impl Drop for SignalMask {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("barbirolli_entrypoint: signal-mask cleanup failed: {error:?}");
        }
    }
}

fn supervised_signal_set() -> SigSet {
    SUPERVISED_SIGNALS.into_iter().collect()
}

fn restore_process_signal_mask(original_bits: u8) -> Result<()> {
    let mut mask =
        SigSet::thread_get_mask().map_err(|source| EntrypointError::ConfigureProcess {
            operation: "signal mask",
            source,
        })?;
    for (index, signal) in SUPERVISED_SIGNALS.iter().enumerate() {
        if original_bits & (1 << index) == 0 {
            mask.remove(*signal);
        } else {
            mask.add(*signal);
        }
    }
    configure!("signal mask", mask.thread_set_mask())
}

pub fn sync_filesystems() {
    sync();
}

/// Powers off the VM after the OCI process exits.
///
/// # Errors
///
/// Returns an error if the power-off system call returns instead of shutting
/// down the VM.
pub fn power_off(exit: ExitStatus) -> Result<()> {
    eprintln!("barbirolli_entrypoint: OCI process exited with {exit}");
    eprintln!("barbirolli_entrypoint: VM starts power-off");
    match reboot(RebootMode::RB_POWER_OFF) {
        Ok(never) => match never {},
        Err(source) => Err(EntrypointError::PowerOff { exit, source }),
    }
}
