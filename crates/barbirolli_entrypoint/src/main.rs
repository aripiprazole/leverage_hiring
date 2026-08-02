use barbirolli_entrypoint::{
    ProcessSpec, Result, mount_userspace_filesystems, power_off, sync_filesystems,
};

fn main() -> Result<()> {
    mount_userspace_filesystems()?;

    let process = ProcessSpec::new("/etc/barbirolli/process.json")?;
    let child = process.spawn()?;

    let exit = child.supervise_and_reap()?;
    sync_filesystems();
    power_off(exit)
}
