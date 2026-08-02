use barbirolli_entrypoint::{
    Result, mount_userspace_filesystems, power_off, read_process_spec, spawn_oci_process,
    sync_filesystems,
};

fn main() -> Result<()> {
    mount_userspace_filesystems()?;

    let process = read_process_spec("/etc/barbirolli/process.json")?;
    let child = spawn_oci_process(process)?;

    let exit = child.supervise_and_reap()?;
    sync_filesystems();
    power_off(exit)
}
