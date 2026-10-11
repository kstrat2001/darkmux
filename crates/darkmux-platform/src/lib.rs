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
