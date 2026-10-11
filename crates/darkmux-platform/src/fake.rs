//! A platform whose every answer is set by the test using it.

use crate::{ContainerHostAccess, Platform};

pub struct FakePlatform {
    pub container_host_access: ContainerHostAccess,
}

impl Platform for FakePlatform {
    fn id(&self) -> &'static str {
        "fake"
    }

    fn container_host_access(&self) -> ContainerHostAccess {
        self.container_host_access
    }
}
