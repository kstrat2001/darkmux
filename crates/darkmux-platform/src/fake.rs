//! A platform whose every answer is set by the test using it.

use crate::{ContainerHostAccess, ContainerUser, Platform};

pub struct FakePlatform {
    pub container_host_access: ContainerHostAccess,
    pub container_user: ContainerUser,
    pub machine_uid: Option<String>,
    pub reclaimable_memory_bytes: Option<u64>,
    /// `(pid, stamp)` pairs [`Platform::process_start_stamp`] answers; any
    /// other pid reads as `None`.
    pub process_start_stamps: Vec<(u32, u64)>,
    pub pid_namespace: Option<String>,
    pub os_release: Option<crate::OsRelease>,
    pub secret_store: crate::SecretStore,
}

/// Every answer at its "nothing to report" value, so a test sets only the
/// answers it is about (`FakePlatform { machine_uid: .., ..Default::default() }`)
/// and a new question does not break every literal.
impl Default for FakePlatform {
    fn default() -> Self {
        FakePlatform {
            container_host_access: ContainerHostAccess::Builtin,
            container_user: ContainerUser::Image,
            machine_uid: None,
            reclaimable_memory_bytes: None,
            process_start_stamps: Vec::new(),
            pid_namespace: None,
            os_release: None,
            secret_store: crate::SecretStore::PrivateFile,
        }
    }
}

impl Platform for FakePlatform {
    fn id(&self) -> &'static str {
        "fake"
    }

    fn container_host_access(&self) -> ContainerHostAccess {
        self.container_host_access
    }

    fn container_user(&self) -> ContainerUser {
        self.container_user
    }

    fn machine_uid(&self) -> Option<String> {
        self.machine_uid.clone()
    }

    /// Fakes report no CPU; a test needing one asserts the strategies directly.
    fn cpu_brand(&self) -> Option<String> {
        None
    }

    fn ram_installed_bytes(&self) -> Option<u64> {
        None
    }

    fn cpu_clusters(&self) -> Vec<crate::CpuClusterInfo> {
        Vec::new()
    }

    fn cpu_ticks(&self) -> Option<Vec<crate::CpuTicks>> {
        None
    }

    fn memory_snapshot(&self) -> Option<crate::MemorySnapshot> {
        None
    }

    fn battery(&self) -> Option<crate::BatteryReading> {
        None
    }

    fn cpu_cluster_mhz(&self) -> Vec<Option<u32>> {
        Vec::new()
    }

    fn package_power_mw(&self) -> Option<f64> {
        None
    }

    fn board_sensors(&self) -> Option<crate::BoardSensors> {
        None
    }

    fn reclaimable_memory_bytes(&self) -> Option<u64> {
        self.reclaimable_memory_bytes
    }

    fn process_start_stamp(&self, pid: u32) -> Option<u64> {
        self.process_start_stamps.iter().find(|(p, _)| *p == pid).map(|(_, s)| *s)
    }

    fn pid_namespace(&self) -> Option<String> {
        self.pid_namespace.clone()
    }

    fn os_release(&self) -> Option<crate::OsRelease> {
        self.os_release.clone()
    }

    fn gpu_api(&self) -> Option<&'static str> {
        None
    }

    fn secret_store(&self) -> crate::SecretStore {
        self.secret_store
    }
}
