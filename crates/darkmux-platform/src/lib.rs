//! Platform strategies: how each host OS answers the questions the rest of
//! darkmux asks about the machine it runs on.
//!
//! Callers ask [`current`] for the [`Platform`] and ask it a question; they
//! never name an OS. Each OS's answers live in its own module, and `current`
//! is the only place a `cfg(target_os)` picks between them. Tests use
//! [`FakePlatform`] instead of the host's.
//!
//! Every strategy module compiles on every OS, so each one's answers are
//! unit-tested everywhere. When a strategy needs an OS-only call (a sysctl, a
//! `/proc` read), that call is `cfg`-gated at the leaf inside its module and
//! its parsing stays compiled everywhere, the way `gestalt_host::mac_probe` does.

mod fake;
mod linux;
mod macos;
mod other;

pub use fake::FakePlatform;
pub use linux::LinuxPlatform;
pub use macos::MacPlatform;
pub use other::OtherPlatform;

/// The questions darkmux asks about the host. One method per question; the
/// answer is data, never an OS name, so a caller can't branch on the OS.
pub trait Platform: Send + Sync {
    /// The strategy's name, for diagnostics (`darkmux doctor`).
    fn id(&self) -> &'static str;

    /// How a dispatch container reaches a server on this machine through the
    /// name `host.docker.internal`.
    fn container_host_access(&self) -> ContainerHostAccess;

    /// Which user a dispatch container runs as, so it can read and write the
    /// host directories bind-mounted into it.
    fn container_user(&self) -> ContainerUser;
}

/// Which user a dispatch container runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerUser {
    /// The image's own user (the runtime image's `USER`). Right where the
    /// engine maps ownership across the mounts (Docker Desktop).
    Image,
    /// A numeric user, and group when given: the host user, where bind mounts
    /// keep the host's ownership (Docker Engine on Linux).
    Host { uid: u32, gid: Option<u32> },
}

impl ContainerUser {
    /// An operator's override: `image`, `<uid>` or `<uid>:<gid>`.
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if value == "image" {
            return Ok(Self::Image);
        }
        let id = |s: &str| s.parse::<u32>().map_err(|_| s.to_string());
        let parsed = match value.split_once(':') {
            None => id(value).map(|uid| Self::Host { uid, gid: None }),
            Some((uid, gid)) => id(uid).and_then(|uid| {
                Ok(Self::Host {
                    uid,
                    gid: Some(id(gid)?),
                })
            }),
        };
        parsed.map_err(|_| {
            format!("`{value}` is not a container user: write `image`, `<uid>` or `<uid>:<gid>` (numbers)")
        })
    }

    /// The value for `docker run --user`, or `None` to leave the image's user.
    pub fn docker_user(&self) -> Option<String> {
        match self {
            Self::Image => None,
            Self::Host {
                uid,
                gid: Some(gid),
            } => Some(format!("{uid}:{gid}")),
            Self::Host { uid, gid: None } => Some(uid.to_string()),
        }
    }
}

/// How a container reaches this machine as `host.docker.internal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerHostAccess {
    /// The container engine defines the name itself (Docker Desktop).
    Builtin,
    /// The name has to be mapped to the engine's host gateway when the
    /// container starts (Docker Engine on Linux).
    HostGateway,
}

/// The strategy for the OS this binary was built for.
pub fn current() -> &'static dyn Platform {
    #[cfg(target_os = "macos")]
    {
        &MacPlatform
    }
    #[cfg(target_os = "linux")]
    {
        &LinuxPlatform
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        &OtherPlatform
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod os_gates;
