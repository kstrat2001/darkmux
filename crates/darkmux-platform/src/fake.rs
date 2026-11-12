//! A platform whose every answer is set by the test using it.

use crate::{ContainerHostAccess, ContainerUser, DefaultSandbox, Platform};

pub struct FakePlatform {
    pub container_host_access: ContainerHostAccess,
    pub container_user: ContainerUser,
    pub default_sandbox: DefaultSandbox,
    pub machine_uid: Option<String>,
    pub reclaimable_memory_bytes: Option<u64>,
    pub memory_pressure: Option<crate::MemoryPressure>,
    /// `(pid, stamp)` pairs [`Platform::process_start_stamp`] answers; any
    /// other pid reads as `None`.
    pub process_start_stamps: Vec<(u32, u64)>,
    /// `(pid, bytes)` pairs [`Platform::process_footprint`] answers.
    pub process_footprints: Vec<(u32, u64)>,
    pub pid_namespace: Option<String>,
    pub confine_support: crate::confine::ConfineSupport,
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
    /// How many [`Platform::tcp_listeners`] reads cannot say (`None`) before
    /// the reads answer; each read counts one down.
    pub tcp_listeners_unreadable_reads: std::sync::atomic::AtomicU32,
    /// The `(peer, pid)` pairs [`Platform::tcp_peers_of`] answers, whatever
    /// the listener.
    pub tcp_peers: Vec<(std::net::SocketAddr, u32)>,
    /// `(port, listeners)` [`Platform::tcp_port_listeners`] answers; any
    /// other port has none.
    pub port_listeners: Vec<(u16, Vec<crate::PortListener>)>,
    /// How many [`Platform::tcp_port_listeners`] reads cannot say (`None`)
    /// before the reads answer; each read counts one down.
    pub port_listeners_unreadable_reads: std::sync::atomic::AtomicU32,
    /// `(pid, group)` pairs [`Platform::process_group`] answers.
    pub process_groups: Vec<(u32, u32)>,
    /// `(pid, app)` pairs [`Platform::signed_app`] names; any other pid is unsigned.
    pub signed_apps: Vec<(u32, crate::SignedApp)>,
    /// What [`Platform::ask_allow`] answers.
    pub allow_answer: crate::AllowAnswer,
}

/// Every answer at its "nothing to report" value, so a test sets only the
/// answers it is about (`FakePlatform { machine_uid: .., ..Default::default() }`)
/// and a new question does not break every literal.
impl Default for FakePlatform {
    fn default() -> Self {
        FakePlatform {
            container_host_access: ContainerHostAccess::Builtin,
            container_user: ContainerUser::Image,
            default_sandbox: DefaultSandbox::Docker,
            machine_uid: None,
            reclaimable_memory_bytes: None,
            memory_pressure: None,
            process_start_stamps: Vec::new(),
            process_footprints: Vec::new(),
            pid_namespace: None,
            confine_support: Default::default(),
            os_release: None,
            secret_store: crate::SecretStore::PrivateFile,
            machine_config: crate::MachineConfig::default(),
            identifying_strings: Vec::new(),
            xdg_data_home: None,
            tcp_listeners: Vec::new(),
            tcp_listeners_unreadable: false,
            tcp_listeners_unreadable_reads: std::sync::atomic::AtomicU32::new(0),
            tcp_peers: Vec::new(),
            port_listeners: Vec::new(),
            port_listeners_unreadable_reads: std::sync::atomic::AtomicU32::new(0),
            process_groups: Vec::new(),
            signed_apps: Vec::new(),
            allow_answer: crate::AllowAnswer::NoDialog("a fake platform shows no dialog".to_string()),
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

    fn default_sandbox(&self) -> DefaultSandbox {
        self.default_sandbox
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

    fn memory_pressure(&self) -> Option<crate::MemoryPressure> {
        self.memory_pressure
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

    fn confine_support(&self) -> crate::confine::ConfineSupport {
        self.confine_support
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
        use std::sync::atomic::Ordering;
        let unreadable_read = self.tcp_listeners_unreadable_reads.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok();
        if self.tcp_listeners_unreadable || unreadable_read {
            return None;
        }
        Some(self.tcp_listeners.iter().find(|(p, _)| *p == port).map(|(_, pids)| pids.clone()).unwrap_or_default())
    }

    fn tcp_peers_of(&self, _local: std::net::SocketAddr) -> Option<Vec<(std::net::SocketAddr, u32)>> {
        Some(self.tcp_peers.clone())
    }

    fn tcp_port_listeners(&self, port: u16) -> Option<Vec<crate::PortListener>> {
        use std::sync::atomic::Ordering;
        if self.port_listeners_unreadable_reads.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
            return None;
        }
        Some(self.port_listeners.iter().find(|(p, _)| *p == port).map(|(_, l)| l.clone()).unwrap_or_default())
    }

    fn process_group(&self, pid: u32) -> Option<u32> {
        self.process_groups.iter().find(|(p, _)| *p == pid).map(|(_, g)| *g)
    }

    fn secret_store(&self) -> crate::SecretStore {
        self.secret_store
    }

    fn signed_app(&self, pid: u32) -> Result<crate::SignedApp, String> {
        self.signed_apps.iter().find(|(p, _)| *p == pid).map(|(_, a)| a.clone()).ok_or_else(|| "unsigned".to_string())
    }

    fn ask_allow(&self, _message: &str, _icon: Option<&std::path::Path>, _timeout_secs: u64) -> crate::AllowAnswer {
        self.allow_answer.clone()
    }

    fn notify(&self, _message: &str) {}
}
