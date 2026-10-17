//! macOS: the platform darkmux was built on.

use crate::{ContainerHostAccess, ContainerUser, Platform};

pub struct MacPlatform;

impl Platform for MacPlatform {
    fn id(&self) -> &'static str {
        "macos"
    }

    /// Docker Desktop defines `host.docker.internal`.
    fn container_host_access(&self) -> ContainerHostAccess {
        ContainerHostAccess::Builtin
    }

    /// Docker Desktop on macOS maps ownership across the mounts, so the image's own
    /// user can use them.
    fn container_user(&self) -> ContainerUser {
        ContainerUser::Image
    }
}
