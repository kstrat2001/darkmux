//! (#3561) The runtimes darkmux supplies to the sandboxes it starts on the
//! host for a workspace (Seatbelt on macOS, Landlock on Linux): dispatch,
//! contained commands and client previews. Required where the project
//! declares one, best effort where it does not (see below). Node.js is the one it ships
//! ([`node`]). A container keeps its image's own runtimes and never comes
//! here.
//!
//! A Mux Bot project is a web app, so Node is a dependency of the project,
//! and darkmux supplies it because darkmux runs the project's code. A fresh
//! Mac has no `node`, and a toolchain under the home (nvm) is not readable
//! in a sandbox on the host.
//!
//! **One mechanism, one entry per runtime.** A [`Runtime`] is data: its
//! pins (rows keyed by runtime, version and platform, each with its
//! archive's URL, SHA-256 and the folder in it that holds `bin`,
//! [`pins::Pin`]), its reader of which version a project asks for
//! ([`Runtime::pick`]: Node's reads `.nvmrc`, `.node-version`, then
//! `engines.node`), and its extras (files darkmux writes into the install
//! and what they add to a sandbox's environment: Node's listen preload).
//! The install ([`install`]), the sandbox grant, the flow records, the
//! preview stage and the doctor rows are the same for every entry in
//! [`RUNTIMES`]. Another runtime is another entry, not another mechanism.
//!
//! - **Install:** on first need, the pinned archive, checked against its
//!   SHA-256 and installed atomically under
//!   `~/.darkmux/toolchains/<runtime>-<version>-<platform>/`.
//! - **Sandbox:** each install's `bin` first on the PATH, its folder
//!   granted read-only ([`Installed::managed`], through
//!   `darkmux_sandbox::toolchain`). No other part of `~/.darkmux` is added to
//!   what a sandbox reads.
//! - **Required only when declared** (Kain's decision): a runtime the
//!   project declares (for Node an `.nvmrc` or `.node-version` naming a
//!   version, even one darkmux cannot read or use (a link, `latest`), but
//!   not an empty or comment-only one; or a readable `package.json`'s
//!   `engines.node`; a `package.json` darkmux cannot read declares
//!   nothing) must be had, or the sandbox
//!   is refused. One it does not declare is best effort: its default build,
//!   or, when that cannot be had (a download or verify failure, no build
//!   pinned for this machine), the sandbox runs without it, with a warning
//!   ([`supply`]). Never a host or unverified binary in its place. With no
//!   managed Node there is no listen preload either: a host-less listen then
//!   binds every address, and darkmux's listener watch (#3559) stops it.
//! - **Reporting:** a fetch writes `toolchain.fetch`, then `toolchain.ready`
//!   or `toolchain.error` (error level when it refuses the sandbox, warn
//!   level when the sandbox goes on without the runtime); a preview also
//!   shows the fetch as a stage, which ends however the fetch ends.

mod install;
pub mod node;
mod node_version;
mod pins;

#[cfg(any(test, feature = "test-support"))]
pub mod fixture;

#[cfg(test)]
mod tests;

pub use install::{
    ensure, installed, remove_unpinned, Installed, Note, DOWNLOAD_BOUND, MARKER, STALE_PARTIAL,
    UNUSED_FOR, USED, USE_STAMP_EVERY,
};
pub use node::NODE;
pub use node_version::{choose, declared_in, unread_package_json, Declared, Request};
pub use pins::{Pin, Version};

use darkmux_flow::payload::{ToolchainPayload, ToolchainUse};
use std::path::Path;

/// A file darkmux writes into a runtime's install, and what it adds to a
/// sandbox's environment.
#[derive(Debug)]
pub struct Extra {
    /// Where, relative to the release folder.
    pub path: &'static str,
    pub content: &'static str,
    /// `(variable, flag)`: `<flag> "<path>"` is appended to the variable in
    /// every sandbox that runs the install (`NODE_OPTIONS`, `--require`).
    pub env: Option<(&'static str, &'static str)>,
}

/// One runtime darkmux manages.
pub struct Runtime {
    /// Its key (`node`): the pins' `runtime`, the install folder's prefix
    /// and the program in its `bin`.
    pub name: &'static str,
    /// How a message names it (`Node`).
    pub display: &'static str,
    /// Every pinned build.
    pub pins: &'static [Pin],
    /// The pin (of this platform's, never empty) a project in these
    /// folders asks for, or why none answers it.
    pub pick: for<'p> fn(&[&Path], &'p [Pin]) -> Result<&'p Pin, String>,
    /// Whether a project in these folders declares a version of it, by the
    /// runtime's own rules (Node: an `.nvmrc` or `.node-version` naming a
    /// version, even one darkmux cannot read or use, but not an empty or
    /// comment-only one; or a readable `package.json`'s `engines.node`).
    /// A declared runtime is required; an undeclared one is best effort.
    pub declares: fn(&[&Path]) -> bool,
    /// What the caller should say about the project's files before any
    /// fetch (Node: a `package.json` darkmux could not read).
    pub notice: fn(&[&Path]) -> Option<String>,
    pub extras: &'static [Extra],
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

/// Every runtime darkmux supplies to a sandbox on the host.
pub const RUNTIMES: &[&Runtime] = &[&NODE];

/// Which builds of a runtime may be installed.
#[derive(Debug, Clone)]
pub struct Source {
    pub pins: Vec<Pin>,
}

impl Source {
    /// The pins compiled into this binary, at their official URLs.
    pub fn official(runtime: &Runtime) -> Source {
        Source {
            pins: runtime.pins.to_vec(),
        }
    }

    /// The source a sandbox start uses: [`Source::official`]. A test build
    /// uses [`fixture::source`] instead, so no test reaches the network.
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn configured(runtime: &Runtime) -> Source {
        Source::official(runtime)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn configured(runtime: &Runtime) -> Source {
        fixture::source(runtime)
    }

    /// The builds pinned for `platform`.
    pub fn pins_for(&self, platform: &str) -> Vec<Pin> {
        self.pins
            .iter()
            .filter(|p| p.platform == platform)
            .copied()
            .collect()
    }
}

/// The way back when a sandbox on the host cannot have its runtime.
fn way_out() -> String {
    format!(
        "To run in a container instead (its image carries its own runtimes), `darkmux config set {} {}`",
        darkmux_sandbox::SANDBOX_KEY,
        darkmux_sandbox::setting_value(darkmux_sandbox::BackendKind::Docker)
    )
}

/// `runtime`'s install for the project in `dirs`, on `platform` (the
/// host's [`darkmux_platform::Platform::node_release_platform`]), from
/// `source`, under `root`; each fetch step through `note`.
pub fn provide(
    runtime: &Runtime,
    platform: Option<&str>,
    root: &Path,
    source: &Source,
    dirs: &[&Path],
    note: &mut dyn FnMut(&Note),
) -> Result<Installed, String> {
    let pin = chosen(runtime, platform, source, dirs)?;
    ensure(runtime, &pin, root, source, note)
}

/// The build of `runtime` the project in `dirs` gets on `platform`, from
/// `source`'s pins, or why none is.
fn chosen(
    runtime: &Runtime,
    platform: Option<&str>,
    source: &Source,
    dirs: &[&Path],
) -> Result<Pin, String> {
    let pins = platform.map(|p| source.pins_for(p)).unwrap_or_default();
    if pins.is_empty() {
        return Err(format!(
            "darkmux pins no {} build for this machine ({}), and a sandbox on the host runs {} from darkmux",
            runtime.display,
            platform.unwrap_or("a CPU or C library, such as musl, that no pinned build runs on"),
            runtime.display,
        ));
    }
    (runtime.pick)(dirs, &pins).copied()
}

/// Where a sandbox start gets its runtimes from: this host's platform, the
/// toolchains folder, each runtime's source, and for its records what
/// needed them and the run's session.
pub struct Supply<'a> {
    pub platform: Option<&'a str>,
    pub root: &'a Path,
    pub source: &'a dyn Fn(&Runtime) -> Source,
    pub needed_by: ToolchainUse,
    pub session: Option<&'a darkmux_types::session_id::SessionId>,
}

/// Every runtime a sandbox for the project in `dirs` gets, as `ask` says.
/// A runtime the project declares is required: one that cannot be had
/// refuses the sandbox (its `toolchain.error` at error level). One it does
/// not declare is best effort: its default build, or, when that cannot be
/// had, nothing (its `toolchain.error` at warn level, once, and a
/// [`Note::Unavailable`]), and the sandbox runs without it. Each step goes to
/// `stage` and its record to `sink`.
pub fn supply(
    ask: &Supply,
    dirs: &[&Path],
    stage: &mut dyn FnMut(&Note),
    sink: &mut dyn FnMut(darkmux_flow::FlowRecord),
) -> Result<Vec<Installed>, String> {
    let mut got = Vec::new();
    for runtime in RUNTIMES {
        got.extend(supply_one(ask, runtime, dirs, stage, sink)?);
    }
    Ok(got)
}

/// [`supply`] for one runtime: `Ok(None)` when it is skipped. Every
/// failure is recorded once: by the fetch's own `toolchain.error` when a
/// fetch failed, else by an [`Note::Unavailable`] record (error level when
/// the project declared the runtime, warn level when it did not).
fn supply_one(
    ask: &Supply,
    runtime: &Runtime,
    dirs: &[&Path],
    stage: &mut dyn FnMut(&Note),
    sink: &mut dyn FnMut(darkmux_flow::FlowRecord),
) -> Result<Option<Installed>, String> {
    let required = (runtime.declares)(dirs);
    if let Some(text) = (runtime.notice)(dirs) {
        stage(&Note::Notice {
            runtime,
            text: &text,
        });
    }
    let mut recorded = false;
    let source = (ask.source)(runtime);
    let pin = chosen(runtime, ask.platform, &source, dirs);
    let got = pin.clone().and_then(|pin| {
        ensure(runtime, &pin, ask.root, &source, &mut |n| {
            recorded |= matches!(n, Note::Failed { .. });
            if let Some(r) = record(n, ask.needed_by, ask.session, required) {
                sink(r);
            }
            stage(n);
        })
    });
    let Err(why) = got else { return got.map(Some) };
    let unavailable = Note::Unavailable {
        runtime,
        pin: pin.as_ref().ok(),
        platform: ask.platform.unwrap_or("unknown"),
        reason: &why,
        required,
    };
    if !recorded {
        if let Some(r) = record(&unavailable, ask.needed_by, ask.session, required) {
            sink(r);
        }
    }
    if required {
        return Err(format!("{why}\n{}", way_out()));
    }
    stage(&unavailable);
    Ok(None)
}

/// [`supply`] for a sandbox on this host: its platform,
/// `~/.darkmux/toolchains`, the configured sources, records to the flow.
pub fn for_sandbox(
    dirs: &[&Path],
    needed_by: ToolchainUse,
    session: Option<&darkmux_types::session_id::SessionId>,
    stage: &mut dyn FnMut(&Note),
) -> Result<Vec<Installed>, String> {
    let root = darkmux_types::config_access::toolchains_dir();
    let source = |r: &Runtime| Source::configured(r);
    let ask = Supply {
        platform: darkmux_platform::current().node_release_platform(),
        root: &root,
        source: &source,
        needed_by,
        session,
    };
    supply(&ask, dirs, stage, &mut |r| {
        let _ = darkmux_flow::record(r);
    })
}

/// What a sandbox's caller says of a step, on its log or stderr: a fetch
/// beginning, and a runtime skipped (with the reason and how to require it).
pub fn said(note: &Note) -> Option<String> {
    match note {
        Note::Fetching { runtime, pin } => {
            Some(format!("getting {} {} for sandboxes on this machine (first use only)", runtime.display, pin.version))
        }
        Note::Unavailable { runtime, reason, required: false, .. } => Some(format!(
            "running this sandbox without darkmux's {0} (or its listen preload): the project declares no {0} \
             version, and darkmux's default {0} could not be had here: {reason}",
            runtime.display
        )),
        Note::Notice { text, .. } => Some(text.to_string()),
        Note::Ready { .. } | Note::Failed { .. } | Note::Unavailable { .. } => None,
    }
}

/// The sandbox seam's view of `installed`: each `bin` first on PATH, each
/// folder read-only, each extra's environment.
pub fn managed(installed: &[Installed]) -> Vec<darkmux_sandbox::toolchain::Managed> {
    installed.iter().map(Installed::managed).collect()
}

/// A step's payload: the build (empty fields where none was chosen, as on
/// a machine with no pinned build), and its time and reason when it ended;
/// `None` for a step that is not a fetch's (a notice).
fn toolchain_payload(note: &Note, needed_by: ToolchainUse) -> Option<ToolchainPayload> {
    let (runtime, pin, platform, elapsed, error) = match note {
        Note::Notice { .. } => return None,
        Note::Fetching { runtime, pin } => (runtime.name, Some(*pin), pin.platform, None, None),
        Note::Ready {
            runtime,
            pin,
            elapsed,
        } => (runtime.name, Some(*pin), pin.platform, Some(*elapsed), None),
        Note::Failed {
            runtime,
            pin,
            elapsed,
            reason,
        } => (
            runtime.name,
            Some(*pin),
            pin.platform,
            Some(*elapsed),
            Some(reason.to_string()),
        ),
        Note::Unavailable {
            runtime,
            pin,
            platform,
            reason,
            ..
        } => (
            runtime.name,
            *pin,
            *platform,
            None,
            Some(reason.to_string()),
        ),
    };
    Some(ToolchainPayload {
        tool: runtime.to_string(),
        version: pin.map(|p| p.version.to_string()).unwrap_or_default(),
        platform: platform.to_string(),
        url: pin.map(|p| p.url.to_string()).unwrap_or_default(),
        needed_by,
        elapsed_ms: elapsed.map(|d| d.as_millis() as u64),
        error,
    })
}

/// A step as its flow record, on `session` when there is one:
/// `toolchain.fetch`, `.ready`, or `.error` (at error level when the
/// runtime is `required` and the sandbox is refused, else at warn level);
/// `None` for a notice, which is said, not recorded.
pub fn record(
    note: &Note,
    needed_by: ToolchainUse,
    session: Option<&darkmux_types::session_id::SessionId>,
    required: bool,
) -> Option<darkmux_flow::FlowRecord> {
    use darkmux_flow::{Level, Payload};
    let payload = toolchain_payload(note, needed_by)?;
    // A failure is an error where it refuses the sandbox (the project
    // declared the runtime) and a warning where the sandbox goes on without
    // it, as `utility.error` is a warning: the work continues.
    let failed = if required { Level::Error } else { Level::Warn };
    let (level, payload) = match note {
        Note::Fetching { .. } => (Level::Info, Payload::ToolchainFetch(payload)),
        Note::Ready { .. } => (Level::Info, Payload::ToolchainReady(payload)),
        Note::Failed { .. } => (failed, Payload::ToolchainError(payload)),
        Note::Unavailable { .. } | Note::Notice { .. } => {
            (failed, Payload::ToolchainError(payload))
        }
    };
    let (category, stage, handle) = (
        darkmux_flow::Category::Machinery,
        darkmux_flow::Stage::Dispatch,
        "darkmux",
    );
    Some(match session {
        Some(s) => {
            darkmux_flow::FlowRecord::for_session_with(s, level, category, stage, payload, handle)
        }
        None => darkmux_flow::FlowRecord {
            ts: darkmux_flow::ts_utc_now(),
            level,
            category,
            tier: darkmux_flow::Tier::Darkmux,
            stage,
            action: payload.action(),
            handle: handle.to_string(),
            phase_id: None,
            session_id: None,
            execution_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: Some(payload),
        },
    })
}

/// What `darkmux doctor` says of one managed runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The default build (for a project that declares no version) is
    /// installed and verified.
    Installed(Installed),
    /// It is fetched on first use.
    WillFetch(Pin),
    /// This machine has no pinned build.
    Unsupported,
}

/// Each runtime's default build for this host, as `darkmux doctor` shows
/// it.
pub fn statuses() -> Vec<(&'static Runtime, Status)> {
    statuses_on(darkmux_platform::current())
}

/// [`statuses`] on `platform` (a test's fake one).
pub fn statuses_on(platform: &dyn darkmux_platform::Platform) -> Vec<(&'static Runtime, Status)> {
    let root = darkmux_types::config_access::toolchains_dir();
    let build = platform.node_release_platform();
    RUNTIMES
        .iter()
        .map(|r| (*r, status_in(r, build, &root, &Source::configured(r))))
        .collect()
}

/// [`statuses`]' answer for `runtime` on `platform`, `root` and `source`.
/// It reads; it writes nothing.
pub fn status_in(
    runtime: &Runtime,
    platform: Option<&str>,
    root: &Path,
    source: &Source,
) -> Status {
    let pins = platform.map(|p| source.pins_for(p)).unwrap_or_default();
    if pins.is_empty() {
        return Status::Unsupported;
    }
    let Ok(pin) = (runtime.pick)(&[], &pins) else {
        return Status::Unsupported;
    };
    let dir = root.join(pin.dir_name());
    if install::verified(&dir, pin) {
        Status::Installed(Installed {
            pin: *pin,
            env: install::extras_env(runtime, &dir),
            dir,
        })
    } else {
        Status::WillFetch(*pin)
    }
}
