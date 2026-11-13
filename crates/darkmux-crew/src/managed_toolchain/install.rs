//! (#3561) Installing a pinned runtime build under `~/.darkmux/toolchains/`,
//! and removing the ones no longer pinned. Nothing here is Node's: a
//! runtime is its pins and its extras ([`Runtime`]).
//!
//! **Atomic.** The archive is downloaded into a fresh hidden sibling
//! (`.<runtime>-<version>-<platform>.<pid>-<nanos>-<n>.partial`), hashed against its
//! pin, unpacked there with the system `tar`, given the runtime's extras
//! (read-only) and marked verified ([`MARKER`], naming the build and its
//! pinned hash) before one `rename` moves the release folder into place. A
//! failed or partial install is never at the final path, and a folder
//! there without the marker is never used (nor deleted: darkmux did not
//! make it). Two first uses at once each stage their own copy; the second
//! `rename` fails against the first one's folder, which it then uses.
//!
//! **Extras** (Node's listen preload) are darkmux's own files, checked by
//! content on every use: one an older darkmux wrote is replaced in place
//! (written beside it, then renamed over it), so an upgrade never needs a
//! new download.
//!
//! **Cleanup is by disuse.** Every use stamps its install ([`USED`]'s
//! modified time, renewed at most once a [`USE_STAMP_EVERY`]), and a new
//! install removes a darkmux install no longer pinned only once nobody has
//! used it for [`UNUSED_FOR`]. So a still-running older darkmux (a
//! `client serve` daemon from before an upgrade) keeps the build its
//! sandboxes run on, and two binaries with different pins never delete
//! each other's builds in turn.
//!
//! **Downloads** go through `curl`, darkmux's HTTPS client, held to https
//! (redirects included) within [`DOWNLOAD_BOUND`].

use super::pins::Pin;
use super::{Runtime, Source};
use crate::bounded_command::{run_bounded, Bounded};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

/// The file in an install that says darkmux verified it: one line naming
/// the build and its pinned SHA-256.
pub const MARKER: &str = ".darkmux-verified";

/// How long one download may take (about 50 MB).
pub const DOWNLOAD_BOUND: Duration = Duration::from_secs(900);

/// How long unpacking may take.
const UNPACK_BOUND: Duration = Duration::from_secs(300);

/// A staging folder older than this is a crashed install's, never a live
/// one's (a live install ends within [`DOWNLOAD_BOUND`] plus the unpack).
pub const STALE_PARTIAL: Duration = Duration::from_secs(24 * 3600);

/// The file in an install whose modified time is its last use.
pub const USED: &str = ".darkmux-used";

/// How stale [`USED`] may be before a use renews it: a sandbox start
/// writes it at most once a day.
pub const USE_STAMP_EVERY: Duration = Duration::from_secs(24 * 3600);

/// How long an unpinned install must go unused before cleanup removes it.
pub const UNUSED_FOR: Duration = Duration::from_secs(7 * 24 * 3600);

/// A verified install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub pin: Pin,
    /// The release folder: `<toolchains>/<runtime>-<version>-<platform>`.
    pub dir: PathBuf,
    /// What its extras add to a sandbox's environment, each value appended
    /// to the variable's own (Node's `NODE_OPTIONS=--require "<preload>"`).
    pub env: Vec<(String, String)>,
}

impl Installed {
    /// Its `bin` folder, which goes first on a sandbox's PATH.
    pub fn bin(&self) -> PathBuf {
        self.dir.join("bin")
    }

    /// The install as the sandbox's toolchain seam takes it: its folder
    /// granted read-only, its `bin` searched first, its extras' environment.
    pub fn managed(&self) -> darkmux_sandbox::toolchain::Managed {
        darkmux_sandbox::toolchain::Managed {
            root: self.dir.clone(),
            bin: self.bin(),
            env: self.env.clone(),
        }
    }
}

/// What an install is doing, for a stage a preview shows and the flow
/// records a run writes. Only a fetch reports: a verified install already
/// there says nothing.
#[derive(Debug, Clone, Copy)]
pub enum Note<'a> {
    /// The download began.
    Fetching { runtime: &'a Runtime, pin: &'a Pin },
    /// The archive matched its pin and is installed.
    Ready {
        runtime: &'a Runtime,
        pin: &'a Pin,
        elapsed: Duration,
    },
    /// Nothing was installed, for `reason`.
    Failed {
        runtime: &'a Runtime,
        pin: &'a Pin,
        elapsed: Duration,
        reason: &'a str,
    },
    /// (#3561) The project declares no version of `runtime` and it could not
    /// be had here (`reason`): the sandbox runs without it. `pin` is the
    /// build that was tried, when one was.
    Skipped {
        runtime: &'a Runtime,
        pin: Option<&'a Pin>,
        platform: &'a str,
        reason: &'a str,
    },
}

/// The marker's line for `pin`.
fn marker_text(pin: &Pin) -> String {
    format!(
        "{} {} {} sha256 {}\n",
        pin.runtime, pin.version, pin.platform, pin.sha256
    )
}

/// Whether `dir` is `pin`'s install, as darkmux verified it: a folder (not a
/// link) holding the marker for exactly this build and `bin/<runtime>`.
pub(super) fn verified(dir: &Path, pin: &Pin) -> bool {
    std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
        && std::fs::read_to_string(dir.join(MARKER)).is_ok_and(|t| t == marker_text(pin))
        && dir.join("bin").join(pin.runtime).is_file()
}

/// What `runtime`'s extras add to a sandbox's environment for an install
/// in `dir`: `<flag> "<path>"` appended to each named variable.
pub(super) fn extras_env(runtime: &Runtime, dir: &Path) -> Vec<(String, String)> {
    runtime
        .extras
        .iter()
        .filter_map(|x| {
            x.env.map(|(var, flag)| {
                (
                    var.to_string(),
                    format!("{flag} \"{}\"", dir.join(x.path).display()),
                )
            })
        })
        .collect()
}

/// Write `runtime`'s extras into `dir` that are missing or not darkmux's
/// current text: each written beside its path and renamed over it,
/// read-only.
fn write_extras(runtime: &Runtime, dir: &Path) -> Result<(), String> {
    for x in runtime.extras {
        let path = dir.join(x.path);
        if std::fs::read_to_string(&path).is_ok_and(|t| t == x.content) {
            continue;
        }
        let parent = path.parent().unwrap_or(dir);
        std::fs::create_dir_all(parent).map_err(|e| format!("making {}: {e}", parent.display()))?;
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = parent.join(format!(".{name}.{}-{n}.new", std::process::id()));
        std::fs::write(&temp, x.content).map_err(|e| format!("writing {}: {e}", temp.display()))?;
        read_only(&temp)?;
        std::fs::rename(&temp, &path).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn read_only(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(not(unix))]
fn read_only(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// `pin`'s verified install under `root`, its extras current, when there
/// is one. A stat and a few small reads: the cost every sandbox start pays
/// once the runtime is there.
pub fn installed(runtime: &Runtime, pin: &Pin, root: &Path) -> Result<Option<Installed>, String> {
    let dir = root.join(pin.dir_name());
    if !verified(&dir, pin) {
        return Ok(None);
    }
    in_use(runtime, pin, dir).map(Some)
}

/// The verified install at `dir`, its extras current and its use stamped.
fn in_use(runtime: &Runtime, pin: &Pin, dir: PathBuf) -> Result<Installed, String> {
    write_extras(runtime, &dir)?;
    stamp_use(&dir, SystemTime::now());
    Ok(Installed {
        pin: *pin,
        env: extras_env(runtime, &dir),
        dir,
    })
}

/// Renew `dir`'s [`USED`] stamp when it is missing or older than
/// [`USE_STAMP_EVERY`] at `now`. Best effort: a stamp that cannot be
/// written leaves the install looking unused sooner, nothing worse.
fn stamp_use(dir: &Path, now: SystemTime) {
    let path = dir.join(USED);
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|at| now.duration_since(at).ok())
        .is_some_and(|age| age < USE_STAMP_EVERY);
    if !fresh {
        let _ = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(now));
    }
}

/// When `dir` was last used: its [`USED`] stamp, else its [`MARKER`]'s
/// time (an install no use has stamped yet).
fn last_use(dir: &Path) -> Option<SystemTime> {
    [USED, MARKER].iter().find_map(|f| {
        std::fs::metadata(dir.join(f))
            .and_then(|m| m.modified())
            .ok()
    })
}

/// `pin`'s install under `root`: the verified one already there, else
/// fetched, checked against the pin and installed atomically, reporting
/// each step through `note`. A failure installs nothing and says why and
/// what to do. A new install removes what `source` no longer pins.
pub fn ensure(
    runtime: &Runtime,
    pin: &Pin,
    root: &Path,
    source: &Source,
    note: &mut dyn FnMut(&Note),
) -> Result<Installed, String> {
    let dir = root.join(pin.dir_name());
    // (#3561 review) Whether anything is there first, then whether it is
    // verified: an install a concurrent first use renames into place
    // between the two is seen whole, never refused as half made.
    if std::fs::symlink_metadata(&dir).is_ok() {
        if verified(&dir, pin) {
            return in_use(runtime, pin, dir);
        }
        return Err(format!(
            "{} is there but is not a {} install darkmux verified (it has no `{MARKER}` naming this build's pinned \
             SHA-256), so darkmux neither uses nor deletes it. Remove it (`rm -rf {}`) and run again: darkmux \
             fetches and verifies {} {} itself",
            dir.display(),
            runtime.display,
            dir.display(),
            runtime.display,
            pin.version
        ));
    }
    std::fs::create_dir_all(root).map_err(|e| format!("making {}: {e}", root.display()))?;
    note(&Note::Fetching { runtime, pin });
    let started = Instant::now();
    let result = staging_dir(root, pin).and_then(|temp| {
        let landed = fetch_into(runtime, pin, &temp).and_then(|inner| land(pin, &inner, &dir));
        let _ = std::fs::remove_dir_all(&temp);
        landed
    });
    let elapsed = started.elapsed();
    match result {
        Ok(()) => {
            note(&Note::Ready {
                runtime,
                pin,
                elapsed,
            });
            remove_unpinned(root, &source.pins, SystemTime::now());
            in_use(runtime, pin, dir)
        }
        Err(reason) => {
            note(&Note::Failed {
                runtime,
                pin,
                elapsed,
                reason: &reason,
            });
            Err(reason)
        }
    }
}

/// A fresh staging folder beside the install, made by this call alone.
fn staging_dir(root: &Path, pin: &Pin) -> Result<PathBuf, String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let temp = root.join(format!(
        ".{}.{}-{nanos:x}-{n}.partial",
        pin.dir_name(),
        std::process::id()
    ));
    std::fs::create_dir(&temp).map_err(|e| format!("making {}: {e}", temp.display()))?;
    Ok(temp)
}

/// Download, check and unpack `pin` into `temp`, give the release folder
/// the runtime's extras and mark it verified: that folder, ready to move
/// into place.
fn fetch_into(runtime: &Runtime, pin: &Pin, temp: &Path) -> Result<PathBuf, String> {
    let archive = temp.join(pin.archive());
    download(pin.url, &archive)?;
    check_hash(runtime, pin, &archive)?;
    unpack(&archive, temp)?;
    let inner = temp.join(pin.top);
    let program = inner.join("bin").join(pin.runtime);
    if !std::fs::symlink_metadata(&program).is_ok_and(|m| m.is_file()) {
        return Err(format!(
            "{} from {} holds no `{}/bin/{}`; nothing was installed",
            pin.archive(),
            pin.url,
            pin.top,
            pin.runtime
        ));
    }
    write_extras(runtime, &inner)?;
    std::fs::write(inner.join(MARKER), marker_text(pin))
        .map_err(|e| format!("writing {}: {e}", inner.join(MARKER).display()))?;
    Ok(inner)
}

/// Move the staged release folder to `dir`, its final path. A concurrent
/// install that got there first leaves its verified folder, which counts.
pub(super) fn land(pin: &Pin, inner: &Path, dir: &Path) -> Result<(), String> {
    match std::fs::rename(inner, dir) {
        Ok(()) => Ok(()),
        Err(_) if verified(dir, pin) => Ok(()),
        Err(e) => Err(format!(
            "installing {} {} at {}: {e}",
            pin.runtime,
            pin.version,
            dir.display()
        )),
    }
}

/// `curl`'s fetch of `url` into `to`: no `~/.curlrc` (`-q`), failing on an
/// HTTP error, only over https (redirects included), or `file:` for a test
/// build's fixture.
fn download(url: &str, to: &Path) -> Result<(), String> {
    // https always; a test build's local fixture is a `file:` URL.
    let scheme = if url.starts_with("file:") {
        "file"
    } else {
        "https"
    };
    let mut cmd = Command::new("curl");
    cmd.arg("-q")
        .args([
            "-fsSL",
            "--proto",
            &format!("={scheme}"),
            "--proto-redir",
            &format!("={scheme}"),
        ])
        .args([
            "--connect-timeout",
            "30",
            "--retry",
            "2",
            "--max-time",
            &DOWNLOAD_BOUND.as_secs().to_string(),
            "-o",
        ])
        .arg(to)
        .arg(url);
    let fix = "Nothing was installed. Check that this machine reaches the download's host (behind a proxy, set \
               `https_proxy`) and run again";
    match run_bounded(cmd, DOWNLOAD_BOUND + Duration::from_secs(30)) {
        Bounded::Finished { success: true, .. } => Ok(()),
        Bounded::Finished { stderr, .. } => Err(format!(
            "could not download {url}: {}. {fix}",
            String::from_utf8_lossy(&stderr).trim()
        )),
        Bounded::TimedOut { seconds } => Err(format!(
            "downloading {url} did not finish within {seconds} s. {fix}"
        )),
        Bounded::Interrupted => Err(format!(
            "downloading {url} was stopped. Nothing was installed"
        )),
        Bounded::SpawnFailed(e) => Err(format!(
            "darkmux downloads with `curl`, which did not start: {e}. Nothing was installed"
        )),
    }
}

/// Refuse an archive whose SHA-256 is not `pin`'s.
fn check_hash(runtime: &Runtime, pin: &Pin, archive: &Path) -> Result<(), String> {
    let got = darkmux_types::sha256::file_hex(archive)
        .map_err(|e| format!("hashing {}: {e}", archive.display()))?;
    if got == pin.sha256 {
        return Ok(());
    }
    Err(format!(
        "{} from {} does not match the SHA-256 darkmux pins for {} {} on {}: it hashed to {got}, the pin is {}. \
         Nothing was installed: the download is not the official release (a corrupted download, a proxy that \
         rewrote it, or a tampered mirror). Run again; if it repeats, check what sits between this machine and \
         the download's host",
        pin.archive(),
        pin.url,
        runtime.display,
        pin.version,
        pin.platform,
        pin.sha256
    ))
}

/// Unpack `archive` into `into` with the system `tar` (bsdtar on a Mac, GNU
/// tar on Linux).
fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    let mut cmd = Command::new("tar");
    cmd.arg("-xzf").arg(archive).arg("-C").arg(into);
    match run_bounded(cmd, UNPACK_BOUND) {
        Bounded::Finished { success: true, .. } => Ok(()),
        Bounded::Finished { stderr, .. } => Err(format!(
            "unpacking {}: {}. Nothing was installed",
            archive.display(),
            String::from_utf8_lossy(&stderr).trim()
        )),
        other => Err(format!(
            "unpacking {} did not finish: {other:?}. Nothing was installed",
            archive.display()
        )),
    }
}

/// Remove from `root` what darkmux installed of a runtime, `pins` no
/// longer names and nobody used for [`UNUSED_FOR`] before `now`, and
/// staging folders a crashed install left over [`STALE_PARTIAL`] before
/// `now`; the paths removed. Only a real folder
/// (never a link) directly in `root` is touched: an install only when it
/// holds darkmux's [`MARKER`] for the runtime its name starts with, a
/// staging folder only by its own name. Anything else there is left alone.
pub fn remove_unpinned(root: &Path, pins: &[Pin], now: SystemTime) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let (path, name) = (
            entry.path(),
            entry.file_name().to_string_lossy().into_owned(),
        );
        let real_dir = std::fs::symlink_metadata(&path).ok().filter(|m| m.is_dir());
        let Some(meta) = real_dir else { continue };
        let gone = stale_partial(&name, &meta, now) || unused_unpinned(&path, &name, pins, now);
        if gone && std::fs::remove_dir_all(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

/// A staging folder (`.<runtime>-...partial`) last written over
/// [`STALE_PARTIAL`] before `now`.
fn stale_partial(name: &str, meta: &std::fs::Metadata, now: SystemTime) -> bool {
    let named = super::RUNTIMES
        .iter()
        .any(|r| name.starts_with(&format!(".{}-", r.name)))
        && name.ends_with(".partial");
    named
        && meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= STALE_PARTIAL)
}

/// A darkmux install (its name a runtime's and its [`MARKER`] that
/// runtime's) that `pins` does not name and nobody used for
/// [`UNUSED_FOR`] before `now`.
fn unused_unpinned(path: &Path, name: &str, pins: &[Pin], now: SystemTime) -> bool {
    let ours = |runtime: &str| {
        name.starts_with(&format!("{runtime}-"))
            && std::fs::read_to_string(path.join(MARKER))
                .is_ok_and(|t| t.starts_with(&format!("{runtime} ")))
    };
    let unused = last_use(path)
        .and_then(|at| now.duration_since(at).ok())
        .is_some_and(|idle| idle >= UNUSED_FOR);
    !pins.iter().any(|p| p.dir_name() == name)
        && super::RUNTIMES.iter().any(|r| ours(r.name))
        && unused
}
