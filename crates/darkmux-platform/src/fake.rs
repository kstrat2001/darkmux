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
    /// `(pid, bytes)` pairs [`Platform::process_footprint`] answers.
    pub process_footprints: Vec<(u32, u64)>,
    pub pid_namespace: Option<String>,
    pub os_release: Option<crate::OsRelease>,
    pub secret_store: crate::SecretStore,
    pub machine_config: crate::MachineConfig,
    pub identifying_strings: Vec<String>,
    pub xdg_data_home: Option<std::path::PathBuf>,
    /// `(port, pids)` [`Platform::tcp_listeners`] answers; any other port
    /// has no listener.
    pub tcp_listeners: Vec<(u16, Vec<u32>)>,
    /// `true`: [`Platform::tcp_listeners`] cannot say (`None`) for any port.
    pub tcp_listeners_unreadable: bool,
    /// `(peer, pid)` pairs [`Platform::tcp_peer_pid`] answers, whatever the
    /// listener; any other peer cannot be told.
    pub tcp_peers: Vec<(std::net::SocketAddr, u32)>,
    /// `(pid, group)` pairs [`Platform::process_group`] answers.
    pub process_groups: Vec<(u32, u32)>,
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
            process_footprints: Vec::new(),
            pid_namespace: None,
            os_release: None,
            secret_store: crate::SecretStore::PrivateFile,
            machine_config: crate::MachineConfig::default(),
            identifying_strings: Vec::new(),
            xdg_data_home: None,
            tcp_listeners: Vec::new(),
            tcp_listeners_unreadable: false,
            tcp_peers: Vec::new(),
            process_groups: Vec::new(),
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

    fn process_footprint(&self, pid: u32) -> Option<u64> {
        self.process_footprints.iter().find(|(p, _)| *p == pid).map(|(_, b)| *b)
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

    fn machine_config(&self) -> crate::MachineConfig {
        self.machine_config.clone()
    }

    fn identifying_strings(&self) -> Vec<String> {
        self.identifying_strings.clone()
    }

    fn xdg_data_home(&self) -> Option<std::path::PathBuf> {
        self.xdg_data_home.clone()
    }

    fn tcp_listeners(&self, port: u16) -> Option<Vec<u32>> {
        if self.tcp_listeners_unreadable {
            return None;
        }
        Some(self.tcp_listeners.iter().find(|(p, _)| *p == port).map(|(_, pids)| pids.clone()).unwrap_or_default())
    }

    fn tcp_peer_pid(&self, _local: std::net::SocketAddr, peer: std::net::SocketAddr) -> Option<u32> {
        self.tcp_peers.iter().find(|(p, _)| *p == peer).map(|(_, pid)| *pid)
    }

    fn process_group(&self, pid: u32) -> Option<u32> {
        self.process_groups.iter().find(|(p, _)| *p == pid).map(|(_, g)| *g)
    }

    fn secret_store(&self) -> crate::SecretStore {
        self.secret_store
    }
}
