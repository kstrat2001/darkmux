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

    /// The `IOPlatformUUID` (`ioreg -rd1 -c IOPlatformExpertDevice`).
    /// Survives renames, hostname changes and OS reinstalls; only a
    /// logic-board swap resets it.
    fn machine_uid(&self) -> Option<String> {
        parse_io_platform_uuid(&ioreg_platform_expert()?)
    }

    /// The `IOPlatformUUID` and `IOPlatformSerialNumber`, the names `scutil`
    /// knows the Mac by (`ComputerName`, `LocalHostName`, `HostName`), the
    /// kernel's host name (`kern.hostname`, which a network may assign) and
    /// the user's full name (`id -F`).
    fn identifying_strings(&self) -> Vec<String> {
        let expert = ioreg_platform_expert().unwrap_or_default();
        let mut out: Vec<String> = ["IOPlatformUUID", "IOPlatformSerialNumber"]
            .iter()
            .filter_map(|k| parse_io_platform_value(&expert, k))
            .collect();
        out.extend(["ComputerName", "LocalHostName", "HostName"].iter().filter_map(|k| scutil_get(k)));
        out.extend([run_trimmed("sysctl", &["-n", "kern.hostname"]), run_trimmed("id", &["-F"])].into_iter().flatten());
        out
    }

    /// `sysctl machdep.cpu.brand_string`.
    fn cpu_brand(&self) -> Option<String> {
        parse_brand_string(&sysctl_brand_string()?)
    }

    /// `sysctl hw.memsize`: the installed memory, exactly.
    fn ram_installed_bytes(&self) -> Option<u64> {
        parse_memsize(&sysctl_memsize()?)
    }

    /// Read through darkmux-crew's mach probe (`host_probe::mach_cpu`) until
    /// that FFI moves here; answering nothing keeps a Mac's lens unchanged.
    fn cpu_clusters(&self) -> Vec<crate::CpuClusterInfo> {
        Vec::new()
    }

    /// See [`Self::cpu_clusters`].
    fn cpu_ticks(&self) -> Option<Vec<crate::CpuTicks>> {
        None
    }

    /// See [`Self::cpu_clusters`]: macOS's memory figures still come from
    /// its own `vm_stat` and mach readers.
    fn memory_snapshot(&self) -> Option<crate::MemorySnapshot> {
        None
    }

    /// Read through darkmux-crew's IOKit battery probe until that moves here.
    fn battery(&self) -> Option<crate::BatteryReading> {
        None
    }

    /// IOReport fills a Mac's cluster frequencies in darkmux-crew's probe.
    fn cpu_cluster_mhz(&self) -> Vec<Option<u32>> {
        Vec::new()
    }

    /// IOReport reads a Mac's power, by rail, in darkmux-crew's probe.
    fn package_power_mw(&self) -> Option<f64> {
        None
    }

    /// A Mac's thermal reading is its pressure state (darkmux-crew's
    /// `thermal` probe); its board sensors are not read.
    fn board_sensors(&self) -> Option<crate::BoardSensors> {
        None
    }

    /// `vm_stat`'s free plus inactive pages, times the page size (`pagesize`,
    /// 16K assumed when unreadable: Apple Silicon's). The reader doctor ran
    /// before #3281, moved.
    fn reclaimable_memory_bytes(&self) -> Option<u64> {
        reclaimable_from_vm_stat(&vm_stat()?, page_size().unwrap_or(16_384))
    }

    /// The kernel's own figures, read by `sysctlbyname` (no process is
    /// spawned, so it is cheap and still answers on a machine short of
    /// memory): `kern.memorystatus_vm_pressure_level`, `vm.swapusage`, and
    /// the compressor's segments and compressed pages against their limits
    /// (`vm.compressor.segment.total` / `.limit`, `vm.compressor.pages_compressed`
    /// / `.pages_compressed_limit`), the two figures a kernel panic report
    /// prints as percentages of their limits.
    fn memory_pressure(&self) -> Option<crate::MemoryPressure> {
        let int = |name: &str| sysctl_bytes(name).and_then(|b| uint_from_sysctl(&b));
        let level = int("kern.memorystatus_vm_pressure_level").and_then(pressure_level_from);
        let swap = sysctl_bytes("vm.swapusage").and_then(|b| swap_from_xsw_usage(&b));
        let compressor_pct = fuller_pct(&[
            (int("vm.compressor.segment.total"), int("vm.compressor.segment.limit")),
            (int("vm.compressor.pages_compressed"), int("vm.compressor.pages_compressed_limit")),
        ]);
        if level.is_none() && swap.is_none() && compressor_pct.is_none() {
            return None;
        }
        Some(crate::MemoryPressure {
            level,
            swap_used_bytes: swap.map(|(used, _)| used),
            swap_free_bytes: swap.map(|(_, free)| free),
            compressor_pct,
        })
    }
    /// `sysctl(CTL_KERN, KERN_PROC, KERN_PROC_PID, pid)`'s
    /// `kinfo_proc.kp_proc.p_starttime`, in microseconds since the epoch.
    /// Works for a process owned by ANY user, which is the point: a reused
    /// pid after a reboot lands on launchd (pid 1) or a root daemon. (#2917
    /// re-review M-A: `proc_pidinfo(PROC_PIDTBSDINFO)` answers nothing for
    /// another user's process.)
    fn process_start_stamp(&self, pid: u32) -> Option<u64> {
        kinfo_start_stamp(pid)
    }

    /// `proc_pid_rusage`'s `ri_phys_footprint` (moved from the residency
    /// ledger, #3448).
    fn process_footprint(&self, pid: u32) -> Option<u64> {
        phys_footprint(pid)
    }

    /// macOS has one pid namespace per machine.
    fn pid_namespace(&self) -> Option<String> {
        Some("macos".to_string())
    }

    /// `sw_vers -productVersion`, installed when `system_profiler`'s install
    /// history last lists `macOS <version>`. The system files' own times are
    /// Apple's build times on the sealed volume, not when an update landed.
    fn os_release(&self) -> Option<crate::OsRelease> {
        let version = command_stdout("sw_vers", &["-productVersion"])?;
        macos_release(&version, || command_stdout("system_profiler", &["SPInstallHistoryDataType", "-json"]).unwrap_or_default())
    }

    /// Apple Silicon runs local models on Metal.
    fn gpu_api(&self) -> Option<&'static str> {
        self.cpu_brand().filter(|b| b.starts_with("Apple M")).map(|_| "Metal")
    }

    /// `hw.perflevelN.name` / `.physicalcpu` (level 0 is the fastest),
    /// `hw.model`, and the GPU's `gpu-core-count` from the AGX accelerator
    /// in the IORegistry. None of them needs root.
    fn machine_config(&self) -> crate::MachineConfig {
        // Separate calls: sysctl exits non-zero when any one key is unknown.
        let model = command_stdout("sysctl", &["hw.model"]).unwrap_or_default();
        let head = command_stdout("sysctl", &["hw.nperflevels"]).unwrap_or_default();
        let levels = sysctl_value(&head, "hw.nperflevels").and_then(|n| n.parse::<usize>().ok()).unwrap_or(0).min(8);
        let keys: Vec<String> =
            (0..levels).flat_map(|i| [format!("hw.perflevel{i}.name"), format!("hw.perflevel{i}.physicalcpu")]).collect();
        let per_level = if levels == 0 {
            String::new()
        } else {
            command_stdout("sysctl", &keys.iter().map(String::as_str).collect::<Vec<_>>()).unwrap_or_default()
        };
        crate::MachineConfig {
            cpu_clusters: perf_levels_from_sysctl(&per_level, levels),
            gpu_cores: command_stdout("ioreg", &["-rc", "AGXAccelerator", "-d1"]).as_deref().and_then(gpu_cores_from_ioreg),
            model_id: sysctl_value(&model, "hw.model").map(str::to_string),
        }
    }

    /// (#3453 review) `lsof -nP -iTCP:<port> -sTCP:LISTEN -Fp` (always in
    /// macOS's base system).
    fn tcp_listeners(&self, port: u16) -> Option<Vec<u32>> {
        lsof_listen_pids(port).map(|out| crate::pids_from_lsof_fp(&out))
    }

    /// One `lsof -nP -iTCP:<port> -sTCP:ESTABLISHED -Fpn`: every process
    /// whose end of a connection runs to `local` (the listener's own ends
    /// are this process, and are skipped by their direction).
    fn tcp_peers_of(&self, local: std::net::SocketAddr) -> Option<Vec<(std::net::SocketAddr, u32)>> {
        lsof_established(local.port()).map(|out| crate::peers_from_lsof_fpn(&out, local))
    }

    fn process_group(&self, pid: u32) -> Option<u32> {
        crate::unix_process_group(pid)
    }

    /// (#3453) The XDG layout, read from this process's environment.
    fn xdg_data_home(&self) -> Option<std::path::PathBuf> {
        crate::xdg_data_home_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
    }

    fn secret_store(&self) -> crate::SecretStore {
        crate::SecretStore::Keychain
    }

    /// `security -i` reads the command from stdin, so the secret is never on
    /// an argv. It can exit 0 when the command failed: the caller reads back.
    fn store_secret(&self, item: &str, account: &str, secret: &str) -> Result<(), String> {
        security_interactive(&security_store_line(item, account, secret)?)
    }

    fn forget_secret(&self, item: &str, account: &str) {
        let _ = command_stdout("security", &["delete-generic-password", "-a", account, "-s", item]);
    }
}

/// The longest command line handed to `security -i`, well inside the ~4095
/// bytes after which it cuts a line and runs the rest as another command.
const SECURITY_LINE_MAX: usize = 2048;

/// The `security -i` line that stores `secret`, or why it is not sent (#3400
/// review): longer than [`SECURITY_LINE_MAX`], or holding a line break, it
/// would run as more than one command. The refusal never names the secret.
pub(crate) fn security_store_line(item: &str, account: &str, secret: &str) -> Result<String, String> {
    let line = format!("add-generic-password -U -a \"{account}\" -s {item} -w \"{secret}\"\n");
    if line.len() > SECURITY_LINE_MAX || line.trim_end_matches('\n').contains(['\n', '\r']) {
        return Err("the keychain command would not be one short line, so it was not run".to_string());
    }
    Ok(line)
}

/// Runs one `security -i` command line, fed on stdin. Its stderr is never
/// read: `security` can echo the command, secret and all.
#[cfg(target_os = "macos")]
fn security_interactive(line: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("running `security`: {e}"))?;
    child.stdin.take().expect("piped").write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("`security` exited with {status}"));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn security_interactive(_line: &str) -> Result<(), String> {
    Err("there is no login keychain on this machine".to_string())
}

#[cfg(target_os = "macos")]
fn command_stdout(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn command_stdout(_cmd: &str, _args: &[&str]) -> Option<String> {
    None
}

/// `lsof`'s `-Fp` answer for the listeners on `port`. It exits 1 with no
/// output when nothing listens: that is an empty answer, not a failure.
#[cfg(target_os = "macos")]
fn lsof_listen_pids(port: u16) -> Option<String> {
    let out = std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"])
        .current_dir("/")
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    match out.status.code() {
        Some(0) => Some(stdout),
        Some(1) if stdout.trim().is_empty() && out.stderr.is_empty() => Some(String::new()),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
fn lsof_listen_pids(_port: u16) -> Option<String> {
    None
}

/// `lsof`'s `-Fpn` answer for the established TCP connections on `port`
/// (exit 1 with no output is "none").
#[cfg(target_os = "macos")]
fn lsof_established(port: u16) -> Option<String> {
    let out = std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:ESTABLISHED", "-Fpn"])
        .current_dir("/")
        .output()
        .ok()?;
    match out.status.code() {
        Some(0 | 1) => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
fn lsof_established(_port: u16) -> Option<String> {
    None
}

/// `macOS <version>` from `sw_vers`' answer, installed when the install
/// history (read only for a version there is) says.
pub(crate) fn macos_release(version: &str, history: impl FnOnce() -> String) -> Option<crate::OsRelease> {
    let version = version.trim();
    if version.is_empty() {
        return None;
    }
    Some(crate::OsRelease { installed_at_unix_ms: macos_installed_at(&history(), version), name: format!("macOS {version}") })
}

/// The newest `install_date` among `system_profiler SPInstallHistoryDataType
/// -json` items named `macOS <version>`. Read by scanning each item's two
/// string fields; this crate parses no JSON otherwise.
pub(crate) fn macos_installed_at(json: &str, version: &str) -> Option<u64> {
    let want = format!("macOS {version}");
    json.split('{')
        .filter(|item| string_field(item, "_name").as_deref() == Some(want.as_str()))
        .filter_map(|item| crate::dates::rfc3339_utc_ms(&string_field(item, "install_date")?))
        .max()
}

/// `"key" : "value"` in a fragment of JSON, the value unescaped of nothing (dates and names here have no escapes).
fn string_field(item: &str, key: &str) -> Option<String> {
    let at = item.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = item[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_string())
}

/// (#2917, moved here by #3200) See [`MacPlatform::process_start_stamp`].
#[cfg(target_os = "macos")]
fn kinfo_start_stamp(pid: u32) -> Option<u64> {
    // `struct kinfo_proc` begins with `struct extern_proc kp_proc`, whose
    // first member is the union `p_un`; `p_starttime` is that union's
    // `struct timeval` arm, so it sits at offset 0 of the whole record:
    // `tv_sec` (i64) at 0, `tv_usec` (i32 on Darwin) at 8. The libc crate
    // does not declare `kinfo_proc` for Apple targets, so the buffer is
    // sized by the kernel's own size probe and read at those offsets.
    let pid = libc::pid_t::try_from(pid).ok()?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut len: libc::size_t = 0;
    // SAFETY: a null destination with a valid `len` pointer is sysctl's
    // documented "how big is it" probe.
    let rc = unsafe {
        libc::sysctl(mib.as_mut_ptr(), mib.len() as libc::c_uint, std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0)
    };
    if rc != 0 || len < 16 {
        return None;
    }
    // u64 words so the i64 at offset 0 is aligned.
    let mut buf = vec![0u64; len.div_ceil(8)];
    let mut got = buf.len() * 8;
    // SAFETY: `buf` is at least `len` bytes (rounded up to whole words) and
    // `got` says exactly that; the return value and filled length are
    // checked before anything is read.
    let rc = unsafe {
        libc::sysctl(mib.as_mut_ptr(), mib.len() as libc::c_uint, buf.as_mut_ptr().cast(), &mut got, std::ptr::null_mut(), 0)
    };
    // A pid with no process answers rc 0 with nothing filled in.
    if rc != 0 || got < 16 {
        return None;
    }
    let head: Vec<u8> = buf[..2].iter().flat_map(|w| w.to_ne_bytes()).collect();
    let sec = i64::from_ne_bytes(head[0..8].try_into().ok()?);
    let usec = i32::from_ne_bytes(head[8..12].try_into().ok()?);
    if sec <= 0 || usec < 0 {
        return None;
    }
    Some((sec as u64).saturating_mul(1_000_000).saturating_add(usec as u64))
}

#[cfg(not(target_os = "macos"))]
fn kinfo_start_stamp(_pid: u32) -> Option<u64> {
    None
}

/// (#1821, moved here from the residency ledger by #3448) `proc_pid_rusage(pid, RUSAGE_INFO_V0, &buf).ri_phys_footprint` — the
/// per-process memory figure Activity Monitor displays and jetsam kills on.
///
/// Called directly rather than shelling out to `/usr/bin/footprint`, which
/// returns the same number: measured at **0.05 s per process** versus a
/// gather that currently completes in ~490 ms and repeats every 5 s. This
/// page's own doctrine (#1286: the observer must not join the observed)
/// makes a ~30% gather-cost increase for three subprocess spawns the wrong
/// trade when a frozen libSystem call costs nothing. `vmmap --summary`
/// yields the same figure at 1.3 s/process and was never a candidate.
///
/// `rusage_info_v0` is a stable, frozen ABI — later flavors append fields
/// and never reorder these. `ri_phys_footprint` is its 8th `u64`.
/// Cross-checked against `/usr/bin/footprint -p` on a live worker: 3.25 GiB
/// both ways, to the byte-rounding shown.
#[cfg(target_os = "macos")]
fn phys_footprint(pid: u32) -> Option<u64> {
    #[repr(C)]
    #[derive(Default)]
    struct RUsageInfoV0 {
        uuid: [u8; 16],
        user_time: u64,
        system_time: u64,
        pkg_idle_wkups: u64,
        interrupt_wkups: u64,
        pageins: u64,
        wired_size: u64,
        resident_size: u64,
        phys_footprint: u64,
        proc_start_abstime: u64,
        proc_exit_abstime: u64,
    }
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut std::ffi::c_void) -> i32;
    }
    let mut buf = RUsageInfoV0::default();
    // SAFETY: `buf` is a correctly-sized, correctly-aligned `#[repr(C)]`
    // mirror of `rusage_info_v0`, and the callee writes only within it. A
    // dead or unreadable pid returns non-zero and we report `None` rather
    // than reading the buffer.
    let Ok(pid) = i32::try_from(pid) else { return None };
    let rc = unsafe { proc_pid_rusage(pid, 0, (&raw mut buf).cast()) };
    (rc == 0).then_some(buf.phys_footprint)
}

#[cfg(not(target_os = "macos"))]
fn phys_footprint(_pid: u32) -> Option<u64> {
    None
}

#[cfg(target_os = "macos")]
fn sysctl_memsize() -> Option<String> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn sysctl_memsize() -> Option<String> {
    None
}

/// The value of `key` in `sysctl <key>...` output (`key: value` lines).
pub(crate) fn sysctl_value<'a>(output: &'a str, key: &str) -> Option<&'a str> {
    output.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')).map(str::trim).filter(|v| !v.is_empty())
}

/// `(name, physical cores)` per perf level, level 0 first. When any level
/// reads no name or no cores, nothing: part of a machine is not its config.
pub(crate) fn perf_levels_from_sysctl(output: &str, levels: usize) -> Vec<(String, u32)> {
    (0..levels)
        .map(|i| {
            let name = sysctl_value(output, &format!("hw.perflevel{i}.name"))?;
            let cores = sysctl_value(output, &format!("hw.perflevel{i}.physicalcpu"))?.parse::<u32>().ok()?;
            (cores > 0).then(|| (name.to_string(), cores))
        })
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default()
}

/// `"gpu-core-count" = 40` in `ioreg -rc AGXAccelerator -d1` output.
pub(crate) fn gpu_cores_from_ioreg(output: &str) -> Option<u32> {
    output.lines().find_map(|l| l.trim().strip_prefix("\"gpu-core-count\" = ")?.trim().parse().ok()).filter(|n| *n > 0)
}

/// `sysctl -n hw.memsize` output: installed bytes.
pub(crate) fn parse_memsize(output: &str) -> Option<u64> {
    output.trim().parse().ok()
}

#[cfg(target_os = "macos")]
fn sysctl_brand_string() -> Option<String> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn sysctl_brand_string() -> Option<String> {
    None
}

/// `sysctl -n machdep.cpu.brand_string` output, trimmed; empty is `None`.
pub(crate) fn parse_brand_string(output: &str) -> Option<String> {
    let s = output.trim();
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(target_os = "macos")]
fn ioreg_platform_expert() -> Option<String> {
    let out = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn ioreg_platform_expert() -> Option<String> {
    None
}

/// Pull the `IOPlatformUUID` value out of `ioreg -rd1 -c
/// IOPlatformExpertDevice` output (a line shaped like
/// `    "IOPlatformUUID" = "XXXXXXXX-...."`). Pure — testable without
/// shelling out.
pub(crate) fn parse_io_platform_uuid(ioreg_output: &str) -> Option<String> {
    parse_io_platform_value(ioreg_output, "IOPlatformUUID")
}

/// A string property of that output by its key. The quoted key is matched,
/// so a sibling like `"IOPlatformUUIDExtra"` can't grab the value.
pub(crate) fn parse_io_platform_value(ioreg_output: &str, key: &str) -> Option<String> {
    let quoted = format!("\"{key}\"");
    let line = ioreg_output.lines().find(|l| l.contains(&quoted))?;
    let eq = line.find('=')?;
    crate::trimmed(line[eq + 1..].trim().trim_matches('"'))
}

fn scutil_get(key: &str) -> Option<String> {
    run_trimmed("scutil", &["--get", key])
}

/// A command's output, trimmed; nothing when it fails or says nothing.
#[cfg(target_os = "macos")]
fn run_trimmed(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    crate::trimmed(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(target_os = "macos"))]
fn run_trimmed(_program: &str, _args: &[&str]) -> Option<String> {
    None
}

/// The raw value of the sysctl `name`; `None` when the kernel has no such
/// name or the read fails.
#[cfg(target_os = "macos")]
fn sysctl_bytes(name: &str) -> Option<Vec<u8>> {
    let name = std::ffi::CString::new(name).ok()?;
    // Every value read here is an integer or `struct xsw_usage` (32 bytes).
    let mut buf = [0u8; 64];
    let mut len: libc::size_t = buf.len();
    // SAFETY: `buf` is `len` bytes, `len` says so, and the kernel writes at
    // most that many and sets `len` to what it wrote; the return value is
    // checked before anything is read.
    let rc = unsafe { libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    (rc == 0 && len <= buf.len()).then(|| buf[..len].to_vec())
}

#[cfg(not(target_os = "macos"))]
fn sysctl_bytes(_name: &str) -> Option<Vec<u8>> {
    None
}

/// An unsigned integer sysctl value: 4 or 8 native-endian bytes.
pub(crate) fn uint_from_sysctl(bytes: &[u8]) -> Option<u64> {
    match bytes.len() {
        4 => Some(u64::from(u32::from_ne_bytes(bytes.try_into().ok()?))),
        8 => Some(u64::from_ne_bytes(bytes.try_into().ok()?)),
        _ => None,
    }
}

/// `kern.memorystatus_vm_pressure_level` to the level it names: 1 normal,
/// 2 warning, 4 critical (the kernel's `kVMPressure*` values, the ones
/// `memory_pressure(1)` reports). Any other value is no reading.
pub(crate) fn pressure_level_from(raw: u64) -> Option<crate::PressureLevel> {
    match raw {
        1 => Some(crate::PressureLevel::Normal),
        2 => Some(crate::PressureLevel::Warning),
        4 => Some(crate::PressureLevel::Critical),
        _ => None,
    }
}

/// `vm.swapusage`'s `struct xsw_usage` to `(used, free)` bytes: its first
/// three members are `xsu_total`, `xsu_avail` and `xsu_used`, each a u64.
pub(crate) fn swap_from_xsw_usage(bytes: &[u8]) -> Option<(u64, u64)> {
    let word = |i: usize| bytes.get(i * 8..i * 8 + 8).and_then(|b| b.try_into().ok()).map(u64::from_ne_bytes);
    let (_total, avail, used) = (word(0)?, word(1)?, word(2)?);
    Some((used, avail))
}

/// The fullest of `(used, limit)` pairs, in percent of its limit, capped at
/// 100. A pair with a figure missing or a zero limit is skipped; `None`
/// when none is left.
pub(crate) fn fuller_pct(pairs: &[(Option<u64>, Option<u64>)]) -> Option<u8> {
    pairs
        .iter()
        .filter_map(|(used, limit)| match (used, limit) {
            (Some(used), Some(limit)) if *limit > 0 => Some((used.saturating_mul(100) / limit).min(100) as u8),
            _ => None,
        })
        .max()
}

#[cfg(target_os = "macos")]
fn vm_stat() -> Option<String> {
    let out = std::process::Command::new("vm_stat").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn vm_stat() -> Option<String> {
    None
}

/// The page size: 16K on Apple Silicon, 4K on Intel.
#[cfg(target_os = "macos")]
fn page_size() -> Option<u64> {
    let out = std::process::Command::new("pagesize").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(not(target_os = "macos"))]
fn page_size() -> Option<u64> {
    None
}

/// Free plus inactive pages from `vm_stat` output, in bytes. A field that is
/// present but unparseable is no figure at all.
pub(crate) fn reclaimable_from_vm_stat(vm_stat: &str, page_size: u64) -> Option<u64> {
    let mut free_pages: u64 = 0;
    let mut inactive_pages: u64 = 0;
    for line in vm_stat.lines() {
        if let Some(rest) = line.strip_prefix("Pages free:") {
            free_pages = parse_pages_field(rest)?;
        } else if let Some(rest) = line.strip_prefix("Pages inactive:") {
            inactive_pages = parse_pages_field(rest)?;
        }
    }
    Some((free_pages + inactive_pages).saturating_mul(page_size))
}

/// A `vm_stat` page count (`"   1234567."`), separators dropped.
pub(crate) fn parse_pages_field(s: &str) -> Option<u64> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    cleaned.parse().ok()
}
