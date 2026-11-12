//! Any OS with no strategy of its own: every answer is what darkmux did
//! before platform strategies existed, so nothing changes there.

use crate::{ContainerHostAccess, ContainerUser, DefaultSandbox, Platform};

pub struct OtherPlatform;

impl Platform for OtherPlatform {
    fn id(&self) -> &'static str {
        "other"
    }

    fn container_host_access(&self) -> ContainerHostAccess {
        ContainerHostAccess::Builtin
    }

    fn container_user(&self) -> ContainerUser {
        ContainerUser::Image
    }

    /// Docker, what darkmux ran before it had a choice.
    fn default_sandbox(&self) -> DefaultSandbox {
        DefaultSandbox::Docker
    }

    fn machine_uid(&self) -> Option<String> {
        None
    }

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
        None
    }

    fn memory_pressure(&self) -> Option<crate::MemoryPressure> {
        None
    }

    fn process_start_stamp(&self, _pid: u32) -> Option<u64> {
        None
    }

    fn process_footprint(&self, _pid: u32) -> Option<u64> {
        None
    }

    fn pid_namespace(&self) -> Option<String> {
        None
    }

    fn os_release(&self) -> Option<crate::OsRelease> {
        None
    }

    fn gpu_api(&self) -> Option<&'static str> {
        None
    }

    fn machine_config(&self) -> crate::MachineConfig {
        crate::MachineConfig::default()
    }

    fn xdg_data_home(&self) -> Option<std::path::PathBuf> {
        None
    }

    fn tcp_listeners(&self, _port: u16) -> Option<Vec<u32>> {
        None
    }

    fn tcp_peers_of(&self, _local: std::net::SocketAddr) -> Option<Vec<(std::net::SocketAddr, u32)>> {
        None
    }

    fn tcp_port_listeners(&self, _port: u16) -> Option<Vec<crate::PortListener>> {
        None
    }

    fn process_group(&self, _pid: u32) -> Option<u32> {
        None
    }

    fn secret_store(&self) -> crate::SecretStore {
        crate::SecretStore::PrivateFile
    }

    fn signed_app(&self, _pid: u32) -> Result<crate::SignedApp, String> {
        Err("darkmux cannot check code signatures on this platform".to_string())
    }

    fn ask_allow(&self, _message: &str, _icon: Option<&std::path::Path>, _timeout_secs: u64) -> crate::AllowAnswer {
        crate::AllowAnswer::NoDialog("darkmux shows no dialogs on this platform".to_string())
    }

    fn notify(&self, _message: &str) {}
}
