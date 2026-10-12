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
}
