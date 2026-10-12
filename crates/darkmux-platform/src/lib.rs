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

    /// This install's stable identity (#640: the identity, distinct from the
    /// operator's display name). `None` when the platform has none to read:
    /// callers treat that as unknown identity and never fall back to a name.
    fn machine_uid(&self) -> Option<String>;

    /// The CPU's marketing name (`"Apple M2"`, `"Apple M5 Max"`), for the
    /// fleet card's hardware line. `None` when the platform can't say:
    /// callers show "not reported", never a placeholder.
    fn cpu_brand(&self) -> Option<String>;

    /// The machine's installed memory, in bytes, as it is sold, for DISPLAY
    /// (the fleet card's `· 8 GB`). Never a planning budget: what darkmux can
    /// actually use is a separate, smaller fact.
    fn ram_installed_bytes(&self) -> Option<u64>;

    /// The CPU's clusters, fastest first (the order macOS's `hw.perflevelN`
    /// uses), for the machine lens. Empty when the platform can't say.
    fn cpu_clusters(&self) -> Vec<CpuClusterInfo>;

    /// Cumulative busy/idle ticks per logical CPU, in CPU-number order; the
    /// lens takes the delta between two reads. `None` when unreadable.
    fn cpu_ticks(&self) -> Option<Vec<CpuTicks>>;

    /// Memory as the machine lens OBSERVES it. Not a planning figure: what
    /// the residency planner may load against is its own, more conservative
    /// question.
    fn memory_snapshot(&self) -> Option<MemorySnapshot>;
}

/// One CPU cluster: `"Performance"`, `"Efficiency"`, or `"CPU"` when there is
/// only one kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuClusterInfo {
    pub name: String,
    pub logical_cpus: usize,
}

/// One logical CPU's cumulative ticks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTicks {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
}

/// A memory reading. Every figure but capacity is optional: a platform
/// reports what it can read, never a stand-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemorySnapshot {
    /// What the OS can use (Linux `MemTotal`), not the installed figure.
    pub capacity_bytes: u64,
    /// Truly free right now.
    pub free_bytes: Option<u64>,
    /// What can be handed out without swapping, reclaimable cache included.
    pub available_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
}

impl MemorySnapshot {
    /// Percent in use: `(capacity - available) / capacity`, rounded.
    pub fn used_pct(&self) -> Option<u64> {
        let available = self.available_bytes?;
        if self.capacity_bytes == 0 {
            return None;
        }
        let used = self.capacity_bytes.saturating_sub(available);
        Some((used * 100 + self.capacity_bytes / 2) / self.capacity_bytes)
    }
}

/// Which user a dispatch container runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerUser {
    /// The image's own user (the runtime image's `USER`). Right where the
    /// engine maps ownership across the mounts (Docker Desktop on macOS).
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
