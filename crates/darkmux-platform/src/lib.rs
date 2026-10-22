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

mod dates;

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

    /// The CPU's tiers (the shape macOS's `hw.perflevelN` reports), fastest
    /// first, for the machine lens; each names its CPUs' positions in
    /// [`Self::cpu_ticks`]. Empty when the platform can't say.
    fn cpu_clusters(&self) -> Vec<CpuClusterInfo>;

    /// Cumulative busy/idle ticks per logical CPU, in CPU-number order; the
    /// lens takes the delta between two reads. `None` when unreadable.
    fn cpu_ticks(&self) -> Option<Vec<CpuTicks>>;

    /// Memory as the machine lens OBSERVES it. Not a planning figure: what
    /// the residency planner may load against is its own, more conservative
    /// question.
    fn memory_snapshot(&self) -> Option<MemorySnapshot>;

    /// The battery's charge facts, for the machine lens. `None` on a machine
    /// with no battery, which is a reading, not a failure.
    fn battery(&self) -> Option<BatteryReading>;

    /// Each cluster's current frequency in MHz, aligned with
    /// [`Platform::cpu_clusters`]. Empty when the platform reports none here.
    fn cpu_cluster_mhz(&self) -> Vec<Option<u32>>;

    /// (#3331) The SoC's power draw right now, in milliwatts, as one total
    /// with no per-rail split. `None` when the platform has no such sensor,
    /// or reads its power another way (macOS: IOReport's rails, in
    /// darkmux-crew's probe).
    fn package_power_mw(&self) -> Option<f64>;

    /// (#3331) The board's labeled temperature sensors and its fan, for the
    /// machine lens. Temperatures, not a thermal pressure level: a reader
    /// must not map them onto one. `None` when the platform reports none.
    fn board_sensors(&self) -> Option<BoardSensors>;

    /// Bytes a model could be loaded into right now without swapping: what
    /// is free plus what the OS hands back on demand. The doctor's `RAM
    /// headroom` figure (#3281). `None` when the platform can't read it:
    /// callers report no figure, never a 0 that reads as "full".
    fn reclaimable_memory_bytes(&self) -> Option<u64>;
    /// (#2917, #3200) An opaque stamp of when process `pid` started, or
    /// `None` when it cannot be read (no such process, or the platform has
    /// no reader). Compare it only for EQUALITY against a stamp this same
    /// method produced for the same pid on this same machine: two equal
    /// stamps mean the pid was not reused in between.
    fn process_start_stamp(&self, pid: u32) -> Option<u64>;

    /// (#3200) Which pid namespace this process lives in: two processes
    /// whose answers differ cannot judge each other's pids. `None` when it
    /// cannot be read; a caller comparing pids then treats them as foreign.
    fn pid_namespace(&self) -> Option<String>;

    /// The OS as a shared result names it (`macOS 15.1`, `Omarchy (Arch
    /// Linux)`), and when that version was installed where the OS says.
    fn os_release(&self) -> Option<OsRelease>;

    /// The GPU API a local model runs on here (`Metal`, `Vulkan`); `None`
    /// when this machine is not one darkmux can say it of.
    fn gpu_api(&self) -> Option<&'static str>;

    /// Where darkmux keeps a secret it is handed (an upload token).
    fn secret_store(&self) -> SecretStore;

    /// Stores `secret` as the login keychain's generic password `item` for
    /// `account`, replacing one there. An error says why it was not stored;
    /// a platform with no keychain (its [`Platform::secret_store`] is a file)
    /// always errs. A caller reads the item back to know it was kept.
    fn store_secret(&self, _item: &str, _account: &str, _secret: &str) -> Result<(), String> {
        Err("this machine keeps secrets in a file, not a keychain".to_string())
    }

    /// Deletes the login keychain's generic password `item` for `account`,
    /// if there is one. Nothing to do where there is no keychain.
    fn forget_secret(&self, _item: &str, _account: &str) {}

    /// The memory size as the machine was sold, in GB, for naming a machine
    /// class: what [`Platform::ram_installed_bytes`] says, where that is exact.
    fn ram_as_sold_gb(&self) -> Option<u64> {
        self.ram_installed_bytes().map(|b| b.div_ceil(1 << 30))
    }
}

/// See [`Platform::os_release`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsRelease {
    pub name: String,
    /// When this version was installed; `None` when the OS does not say, so
    /// a reader cannot claim it held at an earlier time.
    pub installed_at_unix_ms: Option<u64>,
}

/// See [`Platform::secret_store`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretStore {
    /// The login keychain, through `security`.
    Keychain,
    /// A file only the user can read (mode 0600) under `~/.darkmux`.
    PrivateFile,
}

/// A battery's charge facts as the OS reports them. The lens derives its
/// charge state from these with the same rules on every platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryReading {
    pub charge_pct: u8,
    /// A charger is attached (not the negation of `charging`: a full pack
    /// on AC is on AC and not charging).
    pub on_ac: bool,
    /// Current is flowing into the pack.
    pub charging: bool,
    /// The OS says the pack is full; `None` when it doesn't say.
    pub fully_charged: Option<bool>,
    /// Current in milliamps, negative while discharging.
    pub current_ma: Option<i64>,
    /// The OS's time-to-empty estimate; `None` when it has none.
    pub minutes_to_empty: Option<u32>,
}

/// The board's sensors as the OS labels them.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardSensors {
    /// Degrees Celsius by the sensor's own label (`"NAND Flash Temperature"`).
    pub temps_c: std::collections::BTreeMap<String, f64>,
    /// The fastest fan, in rpm; `None` on a fanless machine.
    pub fan_rpm: Option<u32>,
}

/// One CPU cluster: `"Performance"`, `"Efficiency"`, or `"CPU"` when there is
/// only one kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuClusterInfo {
    pub name: String,
    pub logical_cpus: usize,
    /// Where this cluster's CPUs sit in [`Platform::cpu_ticks`]'s list
    /// (positions, not CPU numbers), so a reader sums its own CPUs on any
    /// layout. An M1 Ultra numbers die 0's E and P cores, then die 1's, so
    /// a tier is not one contiguous block.
    pub tick_positions: Vec<usize>,
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
