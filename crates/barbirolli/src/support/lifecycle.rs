use std::{
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use barbirolli::{BalloonConfig, BalloonStatistics, Barbirolli, MemoryMib, VmId, VmStatus};
use tokio::{sync::Barrier, time::timeout};

#[cfg(target_os = "linux")]
use crate::{idle::Sample, lifecycle::BarbirolliVm, vm::managed::HealthError};

#[derive(Clone)]
pub struct VmLifecycleFixture {
    manager: Barbirolli,
    id: VmId,
}

#[derive(Debug, Clone, Copy)]
pub struct HealthMonitorSnapshot {
    pub last_activity: Instant,
    pub strikes: u8,
    pub next_check: Instant,
}

impl VmLifecycleFixture {
    #[must_use]
    pub fn new(manager: Barbirolli, id: VmId) -> Self {
        Self { manager, id }
    }

    #[must_use]
    pub fn status(&self) -> VmStatus {
        self.manager
            .vm(self.id)
            .expect("missing fixture VM")
            .summary()
            .status
    }

    pub async fn balloon_config(&self, inspect: impl FnOnce(&BalloonConfig)) -> BalloonConfig {
        let config = {
            let mut vm = self.manager.vm_mut(self.id).expect("missing fixture VM");
            vm.balloon_config(&self.manager)
                .await
                .expect("failed to read balloon config through fctools")
        };
        inspect(&config);
        config
    }

    pub async fn update_balloon(&self, amount_mib: u16) {
        let mut vm = self.manager.vm_mut(self.id).expect("missing fixture VM");
        vm.update_balloon(&self.manager, MemoryMib::from(amount_mib))
            .await
            .expect("failed to update balloon through fctools");
    }

    pub async fn wait_for_balloon(&self, amount_mib: u32) -> BalloonStatistics {
        timeout(Duration::from_secs(20), async {
            loop {
                let statistics = {
                    let mut vm = self.manager.vm_mut(self.id).expect("missing fixture VM");
                    vm.balloon_statistics(&self.manager)
                        .await
                        .expect("failed to read balloon statistics through fctools")
                };
                if statistics.target_mib == amount_mib && statistics.actual_mib == amount_mib {
                    return statistics;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("balloon did not reach {amount_mib} MiB"))
    }

    #[cfg(target_os = "linux")]
    pub async fn health_sample(&self) -> Sample {
        {
            let mut vm = self.manager.vm_mut(self.id).expect("missing fixture VM");
            let BarbirolliVm::Managed(managed) = &mut *vm else {
                panic!("fixture VM is not running");
            };
            managed
                .flush_metrics_for_test()
                .await
                .expect("failed to flush Firecracker metrics");
        }

        timeout(Duration::from_secs(5), async {
            loop {
                let sample = {
                    let vm = self.manager.vm(self.id).expect("missing fixture VM");
                    let BarbirolliVm::Managed(managed) = &*vm else {
                        panic!("fixture VM is not running");
                    };
                    managed.activity_sample()
                };
                match sample {
                    Ok(sample) => return sample,
                    Err(HealthError::MissingMetrics) => {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(error) => panic!("failed to sample VM health: {error}"),
                }
            }
        })
        .await
        .expect("Firecracker did not emit metrics for the healthchecker")
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn health_sample(&self) -> crate::idle::Sample {
        panic!("VM health samples require Linux")
    }

    #[must_use]
    pub fn health_monitor(&self) -> Option<HealthMonitorSnapshot> {
        #[cfg(target_os = "linux")]
        {
            let vm = self.manager.vm(self.id).expect("missing fixture VM");
            let BarbirolliVm::Managed(managed) = &*vm else {
                return None;
            };
            let monitor = managed
                .monitor
                .lock()
                .expect("the health monitor lock was poisoned");
            monitor.as_ref().map(|monitor| HealthMonitorSnapshot {
                last_activity: monitor.last_activity,
                strikes: monitor.strikes,
                next_check: monitor.next_check,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    pub async fn wait_for_health_monitor(&self) -> HealthMonitorSnapshot {
        self.wait_for_health_monitor_matching("set its baseline", |_| true)
            .await
    }

    pub async fn wait_for_health_activity_after(&self, previous: Instant) -> HealthMonitorSnapshot {
        self.wait_for_health_monitor_matching("observe activity", |monitor| {
            monitor.last_activity > previous && monitor.strikes == 0
        })
        .await
    }

    pub async fn wait_for_health_strikes(&self, strikes: u8) -> HealthMonitorSnapshot {
        self.wait_for_health_monitor_matching("reach the expected strike", |monitor| {
            monitor.strikes == strikes
        })
        .await
    }

    async fn wait_for_health_monitor_matching(
        &self,
        expectation: &str,
        predicate: impl Fn(HealthMonitorSnapshot) -> bool,
    ) -> HealthMonitorSnapshot {
        timeout(Duration::from_secs(20), async {
            loop {
                if let Some(monitor) = self.health_monitor()
                    && predicate(monitor)
                {
                    return monitor;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the healthchecker did not {expectation}"))
    }

    pub async fn wait_for_status(&self, expected: VmStatus) {
        timeout(Duration::from_secs(30), async {
            while self.status() != expected {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("VM did not reach {expected:?}"));
    }

    pub async fn start(&self, inspect: impl FnOnce(&Self)) {
        {
            let mut vm = self.manager.vm_mut(self.id).expect("missing fixture VM");
            vm.start(&self.manager)
                .await
                .expect("failed to start fixture VM");
        }
        inspect(self);
    }

    pub async fn start_concurrently(&self, inspect: impl FnOnce(&Self)) {
        let callers = 2;
        let barrier = Arc::new(Barrier::new(callers + 1));
        let mut tasks = Vec::with_capacity(callers);

        for _ in 0..callers {
            let manager = self.manager.clone();
            let barrier = barrier.clone();
            let id = self.id;
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                let mut vm = manager.vm_mut(id)?;
                vm.start(&manager).await
            }));
        }

        barrier.wait().await;
        for task in tasks {
            task.await
                .expect("a concurrent start task panicked")
                .expect("a concurrent start failed");
        }
        inspect(self);
    }

    pub async fn shutdown(&self, inspect: impl FnOnce(&Self)) {
        {
            let mut vm = self.manager.vm_mut(self.id).expect("missing fixture VM");
            vm.shutdown(&self.manager)
                .await
                .expect("failed to shut down fixture VM");
        }
        inspect(self);
    }

    pub async fn shutdown_concurrently(&self, inspect: impl FnOnce(&Self)) {
        let callers = 2;
        let barrier = Arc::new(Barrier::new(callers + 1));
        let mut tasks = Vec::with_capacity(callers);

        for _ in 0..callers {
            let manager = self.manager.clone();
            let barrier = barrier.clone();
            let id = self.id;
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                let mut vm = manager.vm_mut(id)?;
                vm.shutdown(&manager).await
            }));
        }

        barrier.wait().await;
        for task in tasks {
            task.await
                .expect("a concurrent shutdown task panicked")
                .expect("a concurrent shutdown failed");
        }
        inspect(self);
    }

    pub fn delete_with_timeout(&self) {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let manager = self.manager.clone();
        let id = self.id;
        let delete_thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create DELETE test runtime");
            let result = runtime
                .block_on(manager.delete(id))
                .map_err(|error| error.to_string());
            result_tx
                .send(result)
                .expect("DELETE test receiver dropped");
        });

        result_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("VM deletion deadlocked")
            .expect("failed to delete fixture VM");
        delete_thread.join().expect("VM deletion thread panicked");
    }
}
