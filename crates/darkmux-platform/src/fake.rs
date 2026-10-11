//! A platform whose every answer is set by the test using it.

use crate::{ContainerHostAccess, ContainerUser, Platform};

pub struct FakePlatform {
    pub container_host_access: ContainerHostAccess,
    pub container_user: ContainerUser,
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
}
