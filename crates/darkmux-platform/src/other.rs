//! Any OS with no strategy of its own: every answer is what darkmux did
//! before platform strategies existed, so nothing changes there.

use crate::{ContainerHostAccess, ContainerUser, Platform};

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

    fn reclaimable_memory_bytes(&self) -> Option<u64> {
        None
    }

    fn process_start_stamp(&self, _pid: u32) -> Option<u64> {
        None
    }

    fn pid_namespace(&self) -> Option<String> {
        None
    }
}
