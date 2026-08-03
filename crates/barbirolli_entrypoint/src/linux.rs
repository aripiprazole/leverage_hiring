use std::{
    ffi::{CStr, CString},
    fs, io,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, process::CommandExt as _, process::ExitStatusExt as _},
    },
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    ptr,
};

use oci_spec::runtime::PosixRlimitType;

use crate::{EntrypointError, ProcessSpec, Result};

const SUPERVISED_SIGNALS: [libc::c_int; 7] = [
    libc::SIGCHLD,
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

struct MountSpec {
    source: &'static CStr,
    filesystem: &'static CStr,
    filesystem_name: &'static str,
    target: Target,
    flags: libc::c_ulong,
    data: Option<&'static CStr>,
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

        let target = CString::new(target.as_os_str().as_bytes())
            .expect("static mountpoint paths cannot contain NUL bytes");
        let data = self
            .data
            .map_or(ptr::null(), |options| options.as_ptr().cast());

        // SAFETY: every pointer is backed by a live C string for the duration of
        // the call, and `data` is either null or a live filesystem option string.
        let result = unsafe {
            libc::mount(
                self.source.as_ptr(),
                target.as_ptr(),
                self.filesystem.as_ptr(),
                self.flags,
                data,
            )
        };
        if result == -1 {
            return Err(EntrypointError::MountFilesystem {
                filesystem: self.filesystem_name,
                target: self.target.0,
                source: io::Error::last_os_error(),
            });
        }
        Ok(())
    }
    const USERSPACE: [MountSpec; 7] = [
        MountSpec {
            source: c"proc",
            filesystem: c"proc",
            filesystem_name: "proc",
            target: Target("/proc"),
            flags: (libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC) as libc::c_ulong,
            data: None,
        },
        MountSpec {
            source: c"sysfs",
            filesystem: c"sysfs",
            filesystem_name: "sysfs",
            target: Target("/sys"),
            flags: (libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC) as libc::c_ulong,
            data: None,
        },
        MountSpec {
            source: c"devtmpfs",
            filesystem: c"devtmpfs",
            filesystem_name: "devtmpfs",
            target: Target("/dev"),
            flags: libc::MS_NOSUID as libc::c_ulong,
            data: Some(c"mode=0755"),
        },
        MountSpec {
            source: c"devpts",
            filesystem: c"devpts",
            filesystem_name: "devpts",
            target: Target("/dev/pts"),
            flags: (libc::MS_NOSUID | libc::MS_NOEXEC) as libc::c_ulong,
            data: Some(c"newinstance,ptmxmode=0666,mode=0620,gid=5"),
        },
        MountSpec {
            source: c"tmpfs",
            filesystem: c"tmpfs",
            filesystem_name: "tmpfs",
            target: Target("/dev/shm"),
            flags: (libc::MS_NOSUID | libc::MS_NODEV) as libc::c_ulong,
            data: Some(c"mode=1777"),
        },
        MountSpec {
            source: c"tmpfs",
            filesystem: c"tmpfs",
            filesystem_name: "tmpfs",
            target: Target("/run"),
            flags: (libc::MS_NOSUID | libc::MS_NODEV) as libc::c_ulong,
            data: Some(c"mode=0755"),
        },
        MountSpec {
            source: c"cgroup2",
            filesystem: c"cgroup2",
            filesystem_name: "cgroup2",
            target: Target("/sys/fs/cgroup"),
            flags: (libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC) as libc::c_ulong,
            data: None,
        },
    ];
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

#[derive(Debug)]
struct ChildSettings {
    uid: libc::uid_t,
    gid: libc::gid_t,
    additional_gids: Vec<libc::gid_t>,
    umask: Option<libc::mode_t>,
    rlimits: Vec<RlimitSetting>,
    no_new_privileges: bool,
}

impl ChildSettings {
    fn configure(&self, original_mask: &libc::sigset_t) -> io::Result<()> {
        unsafe {
            let result = libc::pthread_sigmask(libc::SIG_SETMASK, original_mask, ptr::null_mut());
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result));
            }

            bail(libc::setpgid(0, 0))?;
            for setting in &self.rlimits {
                let limit = libc::rlimit {
                    rlim_cur: setting.soft,
                    rlim_max: setting.hard,
                };
                bail(libc::setrlimit(setting.resource, &raw const limit))?;
            }
            bail(libc::setgroups(
                self.additional_gids.len(),
                self.additional_gids.as_ptr(),
            ))?;
            bail(libc::setresgid(self.gid, self.gid, self.gid))?;
            bail(libc::setresuid(self.uid, self.uid, self.uid))?;
            if self.no_new_privileges {
                bail(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
            }
            if let Some(mask) = self.umask {
                libc::umask(mask);
            }
            Ok(())
        }
    }
}

#[derive(Debug)]
struct RlimitSetting {
    resource: RlimitResource,
    soft: libc::rlim_t,
    hard: libc::rlim_t,
}

pub struct OciChild {
    child: Child,
    signal_mask: SignalMask,
}

impl OciChild {
    fn restore_signal_mask(&mut self) -> io::Result<()> {
        self.signal_mask.restore()
    }

    /// Waits for the OCI process while forwarding signals and reaping descendants.
    ///
    /// # Errors
    ///
    /// Returns an error when signal supervision cannot be configured, a signal
    /// cannot be forwarded, or a descendant cannot be reaped.
    pub fn supervise_and_reap(mut self) -> Result<ExitStatus> {
        let process_group = i32::try_from(self.child.id()).map_err(|_| {
            EntrypointError::SignalSupervision(io::Error::new(
                io::ErrorKind::InvalidData,
                "OCI process ID does not fit in pid_t",
            ))
        })?;
        let result = SignalFd::new(&self.signal_mask)?.supervise(process_group);
        let restore = self
            .restore_signal_mask()
            .map_err(EntrypointError::SignalSupervision);

        match (result, restore) {
            (Ok(exit), Ok(())) => Ok(exit),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}

impl ProcessSpec {
    /// Spawns the configured OCI process.
    ///
    /// # Errors
    ///
    /// Returns an error when signal supervision cannot be configured or the OCI
    /// process cannot be spawned.
    pub fn spawn(self) -> Result<OciChild> {
        let ProcessSpec {
            program,
            arguments,
            environment,
            working_directory,
            uid,
            gid,
            additional_gids,
            umask,
            rlimits,
            no_new_privileges,
        } = self;
        let mut command = Command::new(&program);
        command
            .args(arguments)
            .current_dir(working_directory)
            .env_clear()
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        for variable in environment {
            command.env(variable.name, variable.value);
        }

        let settings = ChildSettings {
            uid,
            gid,
            additional_gids,
            umask,
            rlimits: rlimits
                .into_iter()
                .map(|rlimit| RlimitSetting {
                    resource: rlimit_resource(rlimit.resource),
                    soft: rlimit.soft as libc::rlim_t,
                    hard: rlimit.hard as libc::rlim_t,
                })
                .collect(),
            no_new_privileges,
        };

        let signal_mask = SignalMask::block()?;
        let child_signal_mask = signal_mask.original();
        // SAFETY: `configure_child` only makes async-signal-safe syscalls between
        // `fork` and `exec`, as required by `pre_exec`.
        unsafe {
            command.pre_exec(move || settings.configure(&child_signal_mask));
        }
        let child = command
            .spawn()
            .map_err(|source| EntrypointError::SpawnProcess { program, source })?;

        Ok(OciChild { child, signal_mask })
    }
}

fn bail(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_env = "gnu")]
type RlimitResource = libc::__rlimit_resource_t;
#[cfg(not(target_env = "gnu"))]
type RlimitResource = libc::c_int;

fn rlimit_resource(resource: PosixRlimitType) -> RlimitResource {
    match resource {
        PosixRlimitType::RlimitCpu => libc::RLIMIT_CPU,
        PosixRlimitType::RlimitFsize => libc::RLIMIT_FSIZE,
        PosixRlimitType::RlimitData => libc::RLIMIT_DATA,
        PosixRlimitType::RlimitStack => libc::RLIMIT_STACK,
        PosixRlimitType::RlimitCore => libc::RLIMIT_CORE,
        PosixRlimitType::RlimitRss => libc::RLIMIT_RSS,
        PosixRlimitType::RlimitNproc => libc::RLIMIT_NPROC,
        PosixRlimitType::RlimitNofile => libc::RLIMIT_NOFILE,
        PosixRlimitType::RlimitMemlock => libc::RLIMIT_MEMLOCK,
        PosixRlimitType::RlimitAs => libc::RLIMIT_AS,
        PosixRlimitType::RlimitLocks => libc::RLIMIT_LOCKS,
        PosixRlimitType::RlimitSigpending => libc::RLIMIT_SIGPENDING,
        PosixRlimitType::RlimitMsgqueue => libc::RLIMIT_MSGQUEUE,
        PosixRlimitType::RlimitNice => libc::RLIMIT_NICE,
        PosixRlimitType::RlimitRtprio => libc::RLIMIT_RTPRIO,
        PosixRlimitType::RlimitRttime => libc::RLIMIT_RTTIME,
    }
}

struct SignalMask {
    blocked: libc::sigset_t,
    original: libc::sigset_t,
    active: bool,
}

impl SignalMask {
    fn block() -> Result<Self> {
        unsafe {
            let mut blocked = MaybeUninit::<libc::sigset_t>::uninit();
            bail(libc::sigemptyset(blocked.as_mut_ptr()))
                .map_err(EntrypointError::SignalSupervision)?;
            let mut blocked = blocked.assume_init();
            for signal in SUPERVISED_SIGNALS {
                bail(libc::sigaddset(&raw mut blocked, signal))
                    .map_err(EntrypointError::SignalSupervision)?;
            }

            let mut original = MaybeUninit::<libc::sigset_t>::uninit();
            let result =
                libc::pthread_sigmask(libc::SIG_BLOCK, &raw const blocked, original.as_mut_ptr());
            if result != 0 {
                return Err(EntrypointError::SignalSupervision(
                    io::Error::from_raw_os_error(result),
                ));
            }

            Ok(Self {
                blocked,
                original: original.assume_init(),
                active: true,
            })
        }
    }

    fn original(&self) -> libc::sigset_t {
        copy_signal_set(&self.original)
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let result = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &raw const self.original, ptr::null_mut())
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for SignalMask {
    fn drop(&mut self) {
        if let Err(err) = self.restore() {
            eprintln!("barbirolli_entrypoint: signal-mask cleanup failed: {err:?}");
        }
    }
}

fn copy_signal_set(signal_set: &libc::sigset_t) -> libc::sigset_t {
    // SAFETY: `sigset_t` is a plain C value with no destructor. A bitwise copy
    // is the representation-preserving operation required by the libc APIs.
    unsafe { ptr::read(signal_set) }
}

struct SignalFd(OwnedFd);

impl SignalFd {
    fn new(mask: &SignalMask) -> Result<Self> {
        let fd = unsafe { libc::signalfd(-1, &raw const mask.blocked, libc::SFD_CLOEXEC) };
        if fd == -1 {
            return Err(EntrypointError::SignalSupervision(
                io::Error::last_os_error(),
            ));
        }
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn read(&self) -> Result<libc::c_int> {
        let mut info = MaybeUninit::<libc::signalfd_siginfo>::uninit();
        loop {
            let bytes = unsafe {
                libc::read(
                    self.0.as_raw_fd(),
                    info.as_mut_ptr().cast(),
                    size_of::<libc::signalfd_siginfo>(),
                )
            };
            if bytes == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(EntrypointError::SignalSupervision(error));
            }
            if usize::try_from(bytes) == Ok(size_of::<libc::signalfd_siginfo>()) {
                let signal = libc::c_int::try_from(unsafe { info.assume_init() }.ssi_signo)
                    .map_err(|_| {
                        EntrypointError::SignalSupervision(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "signalfd returned a signal number that does not fit in c_int",
                        ))
                    })?;
                return Ok(signal);
            }
            return Err(EntrypointError::SignalSupervision(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "signalfd returned a partial signal record",
            )));
        }
    }

    fn supervise(&self, process_group: libc::pid_t) -> Result<ExitStatus> {
        loop {
            if let Some(exit) = reap_descendants(process_group)? {
                return Ok(exit);
            }

            let signal = self.read()?;
            if signal != libc::SIGCHLD {
                let result = unsafe { libc::kill(-process_group, signal) };
                if result == -1 {
                    let source = io::Error::last_os_error();
                    if source.raw_os_error() != Some(libc::ESRCH) {
                        return Err(EntrypointError::ForwardSignal { signal, source });
                    }
                }
            }
        }
    }
}

fn reap_descendants(main_process: libc::pid_t) -> Result<Option<ExitStatus>> {
    let mut main_exit = None;
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &raw mut status, libc::WNOHANG) };
        if pid > 0 {
            if pid == main_process {
                main_exit = Some(ExitStatus::from_raw(status));
            }
            continue;
        }
        if pid == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
            return Ok(main_exit);
        }

        let source = io::Error::last_os_error();
        if source.kind() != io::ErrorKind::Interrupted {
            return Err(EntrypointError::WaitForProcess(source));
        }
    }
}

pub fn sync_filesystems() {
    unsafe {
        libc::sync();
    }
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
    let result = unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF) };
    debug_assert_eq!(result, -1, "a successful power-off syscall cannot return");
    Err(EntrypointError::PowerOff {
        exit,
        source: io::Error::last_os_error(),
    })
}

#[cfg(test)]
mod tests {
    use oci_spec::runtime::Process;
    use serde_json::json;

    use crate::ProcessSpec;

    #[test]
    #[ignore = "requires root to apply OCI user and supplementary-group settings"]
    fn spawns_and_reaps_the_oci_process() {
        let process: Process = serde_json::from_value(json!({
            "terminal": false,
            "user": { "uid": 0, "gid": 0 },
            "args": ["/bin/sh", "-c", "exit 23"],
            "env": ["PATH=/bin:/usr/bin"],
            "cwd": "/",
            "noNewPrivileges": true
        }))
        .expect("test OCI process should deserialize");
        let process = ProcessSpec::try_from(process).expect("test OCI process should parse");
        let child = process.spawn().expect("OCI process should spawn");
        let exit = child
            .supervise_and_reap()
            .expect("OCI process should be reaped");

        assert_eq!(exit.code(), Some(23));
    }
}
