use std::{
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use barbirolli::{BalloonConfig, BalloonStatistics, Barbirolli, MemoryMib, VmId, VmStatus};
use tokio::sync::Barrier;

#[derive(Clone)]
pub struct VmLifecycleFixture {
    manager: Barbirolli,
    id: VmId,
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
        tokio::time::timeout(Duration::from_secs(20), async {
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
