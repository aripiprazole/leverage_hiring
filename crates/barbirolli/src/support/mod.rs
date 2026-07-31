use std::sync::{
    Mutex, MutexGuard,
    atomic::{AtomicU16, Ordering},
};

use barbirolli::VmId;

#[cfg(target_os = "linux")]
pub mod behavior;
pub mod config;
pub mod firecracker;
pub mod lifecycle;

static FIRECRACKER_TEST_LOCK: Mutex<()> = Mutex::new(());
static NEXT_FIRECRACKER_VM_ID: AtomicU16 = AtomicU16::new(12_000);

pub fn lock_firecracker_tests() -> MutexGuard<'static, ()> {
    FIRECRACKER_TEST_LOCK
        .lock()
        .expect("the Firecracker test lock was poisoned")
}

pub fn next_firecracker_vm_id() -> VmId {
    VmId::try_from(NEXT_FIRECRACKER_VM_ID.fetch_add(1, Ordering::Relaxed))
        .expect("the Firecracker test VM ID pool was exhausted")
}
