//! A platform whose every answer is set by the test using it.

use crate::{ContainerHostAccess, ContainerUser, Platform};

pub struct FakePlatform {
    pub container_host_access: ContainerHostAccess,
    pub container_user: ContainerUser,
    pub machine_uid: Option<String>,
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
}
