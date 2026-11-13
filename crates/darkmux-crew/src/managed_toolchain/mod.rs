//! (#3561) The runtimes darkmux supplies to every sandbox it starts on the
//! host for a workspace (Seatbelt on macOS, Landlock on Linux): dispatch,
//! contained commands and client previews. Node.js is the one it ships
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
//!   `darkmux_sandbox::toolchain`). Nothing else of `~/.darkmux` becomes
//!   readable.
//! - **Reporting:** a fetch writes `toolchain.fetch`, then `toolchain.ready`
//!   or `toolchain.error` ([`for_sandbox`]); a preview also shows it as a
//!   stage. A failure refuses the sandbox with the reason and the fix, and
//!   never falls back to a host or unverified binary.

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
};
pub use node::NODE;
pub use node_version::{choose, declared_in, Declared, Request};
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
    let pins = platform.map(|p| source.pins_for(p)).unwrap_or_default();
    if pins.is_empty() {
        return Err(format!(
            "darkmux pins no {} build for this machine ({}), and a sandbox on the host runs {} from darkmux",
            runtime.display,
            platform.unwrap_or("a CPU no pinned build runs on"),
            runtime.display,
        ));
    }
    let pin = (runtime.pick)(dirs, &pins)?;
    ensure(runtime, pin, root, source, note)
}

/// Every runtime's install for a sandbox on this host for the project in
/// `dirs`: its platform, `~/.darkmux/toolchains`, the configured sources.
/// Each fetch step is written to the flow (on `session` when the sandbox
/// runs for one) and handed to `stage` (a preview's).
pub fn for_sandbox(
    dirs: &[&Path],
    needed_by: ToolchainUse,
    session: Option<&darkmux_types::session_id::SessionId>,
    stage: &mut dyn FnMut(&Note),
) -> Result<Vec<Installed>, String> {
    let root = darkmux_types::config_access::toolchains_dir();
    let platform = darkmux_platform::current().node_release_platform();
    RUNTIMES
        .iter()
        .map(|runtime| {
            provide(
                runtime,
                platform,
                &root,
                &Source::configured(runtime),
                dirs,
                &mut |n| {
                    let _ = darkmux_flow::record(record(n, needed_by, session));
                    stage(n);
                },
            )
            .map_err(|why| format!("{why}\n{}", way_out()))
        })
        .collect()
}

/// The sandbox seam's view of `installed`: each `bin` first on PATH, each
/// folder read-only, each extra's environment.
pub fn managed(installed: &[Installed]) -> Vec<darkmux_sandbox::toolchain::Managed> {
    installed.iter().map(Installed::managed).collect()
}

/// A fetch step as its flow record: `toolchain.fetch`, `.ready` or
/// `.error`, on `session` when there is one.
pub fn record(
    note: &Note,
    needed_by: ToolchainUse,
    session: Option<&darkmux_types::session_id::SessionId>,
) -> darkmux_flow::FlowRecord {
    use darkmux_flow::{Level, Payload};
    let (pin, elapsed, error) = match note {
        Note::Fetching { pin, .. } => (pin, None, None),
        Note::Ready { pin, elapsed, .. } => (pin, Some(*elapsed), None),
        Note::Failed {
            pin,
            elapsed,
            reason,
            ..
        } => (pin, Some(*elapsed), Some(reason.to_string())),
    };
    let payload = ToolchainPayload {
        tool: pin.runtime.to_string(),
        version: pin.version.to_string(),
        platform: pin.platform.to_string(),
        url: pin.url.to_string(),
        needed_by,
        elapsed_ms: elapsed.map(|d| d.as_millis() as u64),
        error,
    };
    let (level, payload) = match note {
        Note::Fetching { .. } => (Level::Info, Payload::ToolchainFetch(payload)),
        Note::Ready { .. } => (Level::Info, Payload::ToolchainReady(payload)),
        Note::Failed { .. } => (Level::Error, Payload::ToolchainError(payload)),
    };
    let (category, stage, handle) = (
        darkmux_flow::Category::Machinery,
        darkmux_flow::Stage::Dispatch,
        "darkmux",
    );
    match session {
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
    }
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
    let root = darkmux_types::config_access::toolchains_dir();
    let platform = darkmux_platform::current().node_release_platform();
    RUNTIMES
        .iter()
        .map(|r| (*r, status_in(r, platform, &root, &Source::configured(r))))
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
