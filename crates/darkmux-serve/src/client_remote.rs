//! (#3560) `client serve`'s remote reach: with `clients.remote` set to
//! `tailscale`, the client port is published on this machine's tailnet over
//! HTTPS by `tailscale serve` (run in the background, proxying to the
//! loopback listener), and removed again on shutdown, Ctrl-C included.
//! darkmux adds no listener off loopback: the tailnet's own daemon terminates
//! TLS and forwards to `127.0.0.1:<port>`.
//!
//! **Reach is not access.** This module only makes the port reachable. The
//! request gate ([`crate::client_pairing`]'s `foreign_request`) still refuses
//! every request that is not from this machine, and a request through
//! `tailscale serve` is not (it carries the proxy's headers), so until remote
//! pairing and the tailnet request path ship (#3560's later PRs), a device on
//! the tailnet that opens the URL is refused like any other.
//!
//! **Only darkmux's own entry is ever changed.** An entry is darkmux's when a
//! record of it is on disk ([`overlay::PublishedRecord`], written before the
//! entry is published, deleted once it is removed) AND the serve
//! configuration still shows it exactly as darkmux published it
//! ([`overlay::AtPort::Exactly`]: one HTTPS port, one handler at `/`, the
//! loopback target, no Funnel). Anything else on the port, the operator's,
//! is never removed or overwritten: darkmux then serves this machine only and
//! says why. An identical entry with no record is used but never removed.
//!
//! **A killed `client serve` leaves its entry behind**, with its record. The
//! next `client serve` on that port (which can only start once the killed
//! one's listener is gone) removes it before publishing again.
//!
//! **Off means untouched.** With `clients.remote` off nothing here runs the
//! tool at all. A record left from an earlier run is only reported, with the
//! command that removes the entry by hand.

use crate::client_wire::ClientRemote;
use darkmux_fleet::overlay::{self, AtPort, PublishedRecord, ServeEntry};
use darkmux_types::config::ClientsRemote;
use std::path::PathBuf;
use std::time::Duration;

/// What the lifecycle runs against: the tool, the directory of records,
/// and the bounds. Production finds them here; tests hand in a fake tool.
#[derive(Debug, Clone)]
pub struct Rig {
    pub bin: String,
    pub records: PathBuf,
    pub timeout: Duration,
    pub publish_timeout: Duration,
    pub pid: u32,
}

impl Rig {
    /// The tool as every darkmux caller finds it, the records under this
    /// machine's darkmux root.
    pub fn production() -> Self {
        let root = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).root;
        Rig {
            bin: overlay::tool_path(),
            records: root.join(overlay::PUBLISHED_DIR),
            timeout: overlay::CALL_TIMEOUT,
            publish_timeout: overlay::PUBLISH_TIMEOUT,
            pid: std::process::id(),
        }
    }

    fn record(&self, port: u16) -> PathBuf {
        overlay::record_path(&self.records, port)
    }
}

/// What a start did: what `whoami` reports, the entry this process must
/// remove on shutdown (only one it published), and the serve-log lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub wire: ClientRemote,
    pub owned: Option<ServeEntry>,
    pub log: Vec<String>,
}

const LOG: &str = "darkmux client serve:";

/// The line that says the URL is reach only, for every published log line.
const NOT_ACCESS: &str = "a request through it is still refused until remote pairing ships";

/// The start, for the listener bound on loopback `port`.
pub fn start(remote: ClientsRemote, port: u16, rig: &Rig) -> Started {
    match remote {
        ClientsRemote::Off => start_off(port, rig),
        ClientsRemote::Tailscale => start_tailnet(port, rig),
    }
}

/// Off: the tool is never run. A record left from an earlier run is said,
/// with the command that removes its entry.
fn start_off(port: u16, rig: &Rig) -> Started {
    let log = overlay::read_record(&rig.record(port))
        .map(|rec| {
            format!(
                "{LOG} a tailnet entry an earlier client serve published ({}) may still be there; clients.remote is off, \
                 so darkmux leaves the tailnet alone. `{}` removes it",
                rec.entry.url(),
                rec.entry.remove_command()
            )
        })
        .into_iter()
        .collect();
    Started { wire: ClientRemote::off(), owned: None, log }
}

/// Not published: `reason` (which names the fix) for `whoami` and the log.
fn unavailable(reason: String, mut log: Vec<String>) -> Started {
    log.push(format!("{LOG} clients.remote is tailscale, but {reason}; serving this machine only"));
    Started { wire: ClientRemote::unavailable(reason), owned: None, log }
}

/// On: the network checked, a stale entry of darkmux's reaped, then the
/// entry published unless something else holds the port.
fn start_tailnet(port: u16, rig: &Rig) -> Started {
    let net = overlay::local_network(&rig.bin, rig.timeout);
    if let Some(u) = net.unusable() {
        return unavailable(format!("{} ({})", u.reason, u.fix), Vec::new());
    }
    let entry = ServeEntry::for_port(net.dns_name.as_deref().unwrap_or_default(), port);
    let mut log = Vec::new();
    let status = match overlay::serve_status(&rig.bin, rig.timeout) {
        Ok(s) => reap_stale(port, rig, s, &mut log),
        Err(e) => return unavailable(format!("{e:#}"), log),
    };
    match overlay::at_port(&status, &entry) {
        Ok(AtPort::Free) => publish_owned(entry, rig, log),
        Ok(AtPort::Exactly) => {
            log.push(format!(
                "{LOG} on the tailnet at {} through an identical entry darkmux did not publish; it is left in place on \
                 shutdown ({NOT_ACCESS})",
                entry.url()
            ));
            Started { wire: ClientRemote::available(entry.url()), owned: None, log }
        }
        Ok(AtPort::Other) => unavailable(
            format!(
                "port {port} on {} is already served by another tailnet entry, which darkmux leaves alone (remove it, \
                 or run `darkmux client serve --port <another>`)",
                entry.host
            ),
            log,
        ),
        Err(e) => unavailable(format!("{e:#}"), log),
    }
}

/// Removes the entry a killed `client serve` left on `port`, when its record
/// is on disk and the tailnet still shows it exactly. Returns the serve
/// configuration as it is now.
fn reap_stale(port: u16, rig: &Rig, status: Vec<u8>, log: &mut Vec<String>) -> Vec<u8> {
    let path = rig.record(port);
    let Some(rec) = overlay::read_record(&path) else { return status };
    match overlay::at_port(&status, &rec.entry) {
        Ok(AtPort::Exactly) => match overlay::remove(&rig.bin, &rec.entry, rig.timeout) {
            Ok(()) => {
                log.push(format!("{LOG} removed the tailnet entry a stopped client serve left ({})", rec.entry.url()));
                let _ = std::fs::remove_file(&path);
                return overlay::serve_status(&rig.bin, rig.timeout).unwrap_or(status);
            }
            Err(e) => log.push(format!(
                "{LOG} could not remove the tailnet entry a stopped client serve left ({}): {e:#}; `{}` removes it",
                rec.entry.url(),
                rec.entry.remove_command()
            )),
        },
        Ok(AtPort::Other) => {
            log.push(format!(
                "{LOG} the tailnet entry a stopped client serve published ({}) has changed since; left in place",
                rec.entry.url()
            ));
            let _ = std::fs::remove_file(&path);
        }
        Ok(AtPort::Free) => {
            let _ = std::fs::remove_file(&path);
        }
        Err(_) => {}
    }
    status
}

/// Records, publishes and checks the entry. The record goes first, so a
/// kill between publish and record can never leave an entry with no record.
fn publish_owned(entry: ServeEntry, rig: &Rig, log: Vec<String>) -> Started {
    let path = rig.record(entry.port);
    let rec = PublishedRecord { entry: entry.clone(), pid: rig.pid };
    if let Err(e) = overlay::write_record(&path, &rec) {
        return unavailable(format!("darkmux could not keep its record of the entry ({e:#}), so it publishes none"), log);
    }
    if let Err(e) = overlay::publish(&rig.bin, &entry, rig.publish_timeout) {
        let mut log = log;
        log.extend(undo(&entry, rig));
        return unavailable(format!("{e:#}"), log);
    }
    let shown = overlay::serve_status(&rig.bin, rig.timeout).ok().and_then(|s| overlay::at_port(&s, &entry).ok());
    if shown != Some(AtPort::Exactly) {
        let mut log = log;
        log.extend(undo(&entry, rig));
        return unavailable(
            format!(
                "`{} serve` did not show the entry as darkmux publishes it after publishing (this Tailscale may read \
                 `{}` differently)",
                overlay::TOOL,
                entry.publish_args().join(" ")
            ),
            log,
        );
    }
    let mut log = log;
    log.push(format!("{LOG} on the tailnet at {} (tailscale serve; {NOT_ACCESS})", entry.url()));
    Started { wire: ClientRemote::available(entry.url()), owned: Some(entry), log }
}

/// After a publish that failed or did not show: an entry that is there
/// exactly is removed ([`stop`]); otherwise the record goes, unless the
/// tailnet could not be read (a publish cut off by its bound may still have
/// landed, so the next start looks again).
fn undo(entry: &ServeEntry, rig: &Rig) -> Vec<String> {
    match overlay::serve_status(&rig.bin, rig.timeout).ok().map(|s| overlay::at_port(&s, entry)) {
        Some(Ok(AtPort::Exactly)) => stop(entry, rig),
        Some(Ok(_)) => {
            let _ = std::fs::remove_file(rig.record(entry.port));
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Removes `entry`, which this process published, if the tailnet still shows
/// it exactly; its record is deleted unless the removal failed (then the
/// next start retries). Returns the serve-log lines.
pub fn stop(entry: &ServeEntry, rig: &Rig) -> Vec<String> {
    let path = rig.record(entry.port);
    let status = match overlay::serve_status(&rig.bin, rig.timeout) {
        Ok(s) => s,
        Err(e) => {
            return vec![format!(
                "{LOG} could not read the tailnet's serve entries ({e:#}); the next client serve removes its entry, or `{}` does",
                entry.remove_command()
            )]
        }
    };
    let line = match overlay::at_port(&status, entry) {
        Ok(AtPort::Exactly) => match overlay::remove(&rig.bin, entry, rig.timeout) {
            Ok(()) => format!("{LOG} removed its tailnet entry ({})", entry.url()),
            Err(e) => {
                return vec![format!(
                    "{LOG} could not remove its tailnet entry ({e:#}); the next client serve removes it, or `{}` does",
                    entry.remove_command()
                )]
            }
        },
        Ok(AtPort::Free) => format!("{LOG} its tailnet entry was already gone"),
        Ok(AtPort::Other) => format!("{LOG} its tailnet entry ({}) changed since it was published; left in place", entry.url()),
        Err(e) => return vec![format!("{LOG} could not read the tailnet's serve entries ({e:#}); `{}` removes its entry", entry.remove_command())],
    };
    let _ = std::fs::remove_file(&path);
    vec![line]
}

#[cfg(test)]
#[path = "client_remote_tests.rs"]
mod tests;
