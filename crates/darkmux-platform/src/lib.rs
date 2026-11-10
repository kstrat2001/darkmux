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
    /// How hard the machine's memory is pressed right now: the kernel's own
    /// pressure level, the swap in use and left, and how full the compressor
    /// is against the kernel's limit for it. Each figure is `None` where the
    /// platform does not report it; the whole answer is `None` where it
    /// reads none of them. Cheap enough to ask several times a second: a
    /// start is watched with it while its memory is committed.
    fn memory_pressure(&self) -> Option<MemoryPressure>;
    /// (#2917, #3200) An opaque stamp of when process `pid` started, or
    /// `None` when it cannot be read (no such process, or the platform has
    /// no reader). Compare it only for EQUALITY against a stamp this same
    /// method produced for the same pid on this same machine: two equal
    /// stamps mean the pid was not reused in between.
    fn process_start_stamp(&self, pid: u32) -> Option<u64>;
    /// (#3448) How much memory process `pid` holds, in bytes: macOS's
    /// physical footprint (what Activity Monitor's Memory column shows, which
    /// counts a Metal model's GPU allocations that RSS does not). `None` for
    /// no such process, or where the platform has no reader.
    fn process_footprint(&self, pid: u32) -> Option<u64>;

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

    /// What this machine was bought with beyond its chip and RAM: the CPU
    /// and GPU core counts and the model, the choices a buyer makes. What
    /// the OS does not say is left empty, never guessed.
    fn machine_config(&self) -> MachineConfig;

    /// Where darkmux keeps a secret it is handed (an upload token).
    fn secret_store(&self) -> SecretStore;

    /// (#3453) Where a tool that follows the XDG base-directory layout keeps
    /// per-user data: `$XDG_DATA_HOME` when it is an absolute path, else
    /// `~/.local/share` ([`xdg_data_home_from`]). rec (the Flint engine)
    /// installs there on macOS and Linux alike. `None` where darkmux knows of
    /// no such layout, or there is no home directory.
    fn xdg_data_home(&self) -> Option<std::path::PathBuf>;

    /// (#3453 review) The processes listening for TCP on `port` (bound to
    /// loopback or every address): `Some(vec![])` when none does, `None`
    /// when this platform cannot say. What tells a server darkmux started
    /// from another answering on the same port under the same model name.
    fn tcp_listeners(&self, port: u16) -> Option<Vec<u32>>;

    /// The process on this machine at the other end of a loopback TCP
    /// connection to this process's listener `local`, for every such
    /// connection at once: `(peer, pid)`, where `peer` is the other end's
    /// own address (a process this user cannot see is left out). `None` when
    /// the platform cannot say. What tells apart the processes that connect
    /// to a loopback listener, since every container's connection to this
    /// machine arrives from the one process forwarding it (Docker's); one
    /// call answers every connection, so a flood costs one read, not one per
    /// connection.
    fn tcp_peers_of(&self, local: std::net::SocketAddr) -> Option<Vec<(std::net::SocketAddr, u32)>>;

    /// The process group `pid` is in; `None` for no such process, or where
    /// the platform has no reader.
    fn process_group(&self, pid: u32) -> Option<u32>;

    /// What would identify this machine if it were shared: its hardware id,
    /// its serial number and its names on the network. darkmux never sends
    /// these; what it shares is checked against them. Empty where none is read.
    fn identifying_strings(&self) -> Vec<String> {
        Vec::new()
    }

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

/// See [`Platform::machine_config`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachineConfig {
    /// Physical cores per CPU cluster, by the OS's own cluster name
    /// (`Super`, `Performance`, `Efficiency`), fastest first. Empty when not read.
    pub cpu_clusters: Vec<(String, u32)>,
    pub gpu_cores: Option<u32>,
    /// The model identifier the Mac reports (`Mac16,5`).
    pub model_id: Option<String>,
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

/// The kernel's own verdict on memory pressure, mildest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PressureLevel {
    Normal,
    Warning,
    Critical,
}

/// A memory pressure reading ([`Platform::memory_pressure`]). Every figure
/// is optional: a platform reports what it can read, never a stand-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryPressure {
    pub level: Option<PressureLevel>,
    pub swap_used_bytes: Option<u64>,
    /// Swap left in what is allocated. Where swap grows on demand (macOS
    /// adds swap files as it needs them) this is not headroom.
    pub swap_free_bytes: Option<u64>,
    /// How full the compressor is against the kernel's own limits for it,
    /// in percent: the fuller of its segments and its compressed pages.
    pub compressor_pct: Option<u8>,
    /// How full the compressor's segments are against the kernel's limit
    /// for them, in percent: the figure a kernel panic report prints as
    /// "% of segments limit".
    pub compressor_segments_pct: Option<u8>,
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

/// [`Platform::xdg_data_home`] from the two variables it reads: an absolute
/// `XDG_DATA_HOME`, else `<HOME>/.local/share`. A relative or empty
/// `XDG_DATA_HOME` is ignored, as the base-directory spec says.
pub fn xdg_data_home_from(
    xdg_data_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    let xdg = xdg_data_home.map(std::path::PathBuf::from).filter(|p| p.is_absolute());
    xdg.or_else(|| home.filter(|h| !h.is_empty()).map(|h| std::path::PathBuf::from(h).join(".local").join("share")))
}

/// The pids in `lsof -Fp`'s answer: one `p<pid>` line per process; every
/// other field line is skipped.
pub(crate) fn pids_from_lsof_fp(out: &str) -> Vec<u32> {
    out.lines().filter_map(|l| l.strip_prefix('p')).filter_map(|p| p.trim().parse().ok()).collect()
}

/// Every `(peer, pid)` in `lsof -Fpn`'s answer whose socket is connected
/// to `local` (a `n<peer>-><local>` line under its `p<pid>` line). The
/// listener's own ends (`n<local>-><peer>`) are not peers.
pub(crate) fn peers_from_lsof_fpn(out: &str, local: std::net::SocketAddr) -> Vec<(std::net::SocketAddr, u32)> {
    let to = format!("->{local}");
    let mut pid = None;
    let mut peers = Vec::new();
    for line in out.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.trim().parse().ok();
        } else if let Some(peer) = line.strip_prefix('n').and_then(|n| n.strip_suffix(to.as_str())) {
            if let (Ok(peer), Some(pid)) = (peer.parse(), pid) {
                peers.push((peer, pid));
            }
        }
    }
    peers
}

/// `getpgid(pid)`, shared by the Unix strategies.
#[cfg(unix)]
pub(crate) fn unix_process_group(pid: u32) -> Option<u32> {
    let pid = i32::try_from(pid).ok().filter(|p| *p > 0)?;
    let group = unsafe { libc::getpgid(pid) };
    (group > 0).then_some(group as u32)
}

#[cfg(not(unix))]
pub(crate) fn unix_process_group(_pid: u32) -> Option<u32> {
    None
}

/// A value as a file or a command gives it: trimmed of whitespace and the
/// device tree's trailing NUL. Empty is `None`.
pub(crate) fn trimmed(value: &str) -> Option<String> {
    let s = value.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    (!s.is_empty()).then(|| s.to_string())
}
