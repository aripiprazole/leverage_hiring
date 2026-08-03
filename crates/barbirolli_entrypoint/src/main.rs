use barbirolli_entrypoint::{
    ProcessSpec, Result, mount_proc_filesystem, mount_userspace_filesystems, power_off,
    run_oci_process_if_requested, sync_filesystems,
};

fn main() -> Result<()> {
    mount_proc_filesystem()?;
    pid1::relaunch_if_pid1()?;
    run_oci_process_if_requested()?;

    mount_userspace_filesystems()?;
    eprintln!("barbirolli_entrypoint: userspace filesystems are ready");

    let process = ProcessSpec::new("/etc/barbirolli/process.json")?;
    let child = process.spawn()?;
    eprintln!("barbirolli_entrypoint: OCI process started");

    let exit = child.supervise_and_reap()?;
    sync_filesystems();
    power_off(exit)
}
