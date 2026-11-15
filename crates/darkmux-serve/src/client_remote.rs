//! (#3560) `client serve`'s remote reach: with `clients.remote` set to
//! `tailscale`, the client port is published on this machine's tailnet over
//! HTTPS by `tailscale serve` (run in the background, proxying to the
//! loopback listener), and removed again on shutdown, Ctrl-C included.
//! darkmux adds no listener off loopback: the tailnet's own daemon terminates
//! TLS and forwards to `127.0.0.1:<port>`.
//!
//! **Reach, then access.** This module only makes the port reachable, and
//! says which entry requests arrive through ([`Started::serving`]). The
//! request gate ([`crate::client_pairing`]'s `admitted`) answers a request
//! through that entry only from a paired device: its bearer token, and the
//! tailnet user and device recorded for that token ([`crate::client_tailnet`]);
//! a device pairs through it only with an approval on this Mac. With no entry
//! served, every request not from this machine is refused.
//!
//! **Only darkmux's own entry is ever changed.** An entry is darkmux's when a
//! record of it is on disk ([`overlay::PublishedRecord`], written before the
//! entry is published, deleted once it is removed) AND the serve
//! configuration still shows it exactly as darkmux published it
//! ([`overlay::AtPort::Exactly`]: one HTTPS port, one handler at `/`, the
//! loopback target, no Funnel). Anything else on the port, the operator's,
//! is never removed or overwritten: darkmux then serves this machine only and
//! says why. An identical entry with no record is used but never removed. A
//! publish that lands in a shape darkmux did not ask for is left up, and the
//! log names it and the command that removes it: darkmux cannot tell that
//! shape from an operator's entry, so it does not remove it.
//!
//! **A killed `client serve` leaves its entry behind**, with its record. The
//! next `client serve` on that port (which can only start once the killed
//! one's listener is gone) removes it before publishing again.
//!
//! **Off never publishes, and runs the tool only to remove an entry its own
//! record proves it left.** With `clients.remote` off and no record, nothing
//! here runs the tool. With a record, a read of the machine's current name
//! and one of `serve status`, and one removal only when the tailnet shows
//! that entry exactly under that name; then the record goes. (This is `client serve` alone: `darkmux doctor`, viewer links in
//! fleet mode and the fleet identity provider run the tool on their own.)
//!
//! **Removal names its mount** (`--set-path=/`, [`ServeEntry::remove_args`]):
//! with none, Tailscale removes every handler on the port. And it always
//! acts on the machine's current name, so an entry left under a name the
//! machine had before a rename is reported with remedies that work
//! ([`overlay::renamed_reason`]), never retried.

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

/// What a start did: what `whoami` reports, the entry paired devices'
/// requests arrive through (the one published, or an identical one found),
/// the entry this process must remove on shutdown (only one it published),
/// and the serve-log lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub wire: ClientRemote,
    pub serving: Option<ServeEntry>,
    pub owned: Option<ServeEntry>,
    pub log: Vec<String>,
}

const LOG: &str = "darkmux client serve:";

/// The line that says who the URL answers, for every published log line.
const NOT_ACCESS: &str = "a device pairs through it only with an approval on this Mac, and is then answered with its \
                          token, as the tailnet user and from the device it paired as";

/// The start, for the listener bound on loopback `port`.
pub fn start(remote: ClientsRemote, port: u16, rig: &Rig) -> Started {
    match remote {
        ClientsRemote::Off => start_off(port, rig),
        ClientsRemote::Tailscale => start_tailnet(port, rig),
    }
}

/// Off: never publishes. A record left from an earlier run gets one read of
/// this machine's current name and one `serve status` read and, when the
/// tailnet shows its entry exactly under that name, one removal ([`reap`]);
/// an entry under an earlier name is reported, never removed. No record, no
/// tool call.
fn start_off(port: u16, rig: &Rig) -> Started {
    let log = match overlay::read_record(&rig.record(port)) {
        Some(rec) => reap_while_off(&rec, rig),
        None => Vec::new(),
    };
    Started { wire: ClientRemote::off(), serving: None, owned: None, log }
}

/// [`start_off`]'s reap: nothing is removed unless both reads answer.
fn reap_while_off(rec: &PublishedRecord, rig: &Rig) -> Vec<String> {
    let cannot = |what: &str, e: String| {
        vec![format!(
            "{LOG} clients.remote is off; could not read {what} ({e}), so the tailnet entry an earlier client serve left \
             ({}) stays, with its record, for the next start",
            rec.entry.url()
        )]
    };
    let current = match overlay::current_name(&rig.bin, rig.timeout) {
        Ok(name) => name,
        Err(e) => return cannot("this machine's tailnet name", e),
    };
    let status = match overlay::serve_status(&rig.bin, rig.timeout) {
        Ok(s) => s,
        Err(e) => return cannot("the tailnet's serve entries", format!("{e:#}")),
    };
    let mut log = Vec::new();
    if let Reaped::Blocked(reason) = reap(rec, &status, &current, rig, &mut log) {
        log.push(format!("{LOG} clients.remote is off, and {reason}"));
    }
    log
}

/// Not published: `reason` (which names the fix) for `whoami` and the log.
fn unavailable(reason: String, mut log: Vec<String>) -> Started {
    log.push(format!("{LOG} clients.remote is tailscale, but {reason}; serving this machine only"));
    Started { wire: ClientRemote::unavailable(reason), serving: None, owned: None, log }
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
    let status = match serve_status_after_reap(port, &entry.host, rig, &mut log) {
        Ok(s) => s,
        Err(reason) => return unavailable(reason, log),
    };
    match overlay::at_port(&status, &entry) {
        Ok(AtPort::Free) => publish_owned(entry, rig, log),
        Ok(AtPort::Exactly) => {
            log.push(format!(
                "{LOG} on the tailnet at {} through an identical entry darkmux did not publish; it is left in place on \
                 shutdown ({NOT_ACCESS})",
                entry.url()
            ));
            Started { wire: ClientRemote::available(entry.url()), serving: Some(entry), owned: None, log }
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

/// The serve configuration, after the entry a killed `client serve` left on
/// `port` (its record on disk) was dealt with ([`reap`]); `Err` with the
/// reason when it cannot be read, or the left entry is under an earlier
/// name of this machine (`host` is the current one).
fn serve_status_after_reap(port: u16, host: &str, rig: &Rig, log: &mut Vec<String>) -> Result<Vec<u8>, String> {
    let status = overlay::serve_status(&rig.bin, rig.timeout).map_err(|e| format!("{e:#}"))?;
    let Some(rec) = overlay::read_record(&rig.record(port)) else { return Ok(status) };
    match reap(&rec, &status, host, rig, log) {
        Reaped::Blocked(reason) => Err(reason),
        Reaped::Removed => overlay::serve_status(&rig.bin, rig.timeout).map_err(|e| format!("{e:#}")),
        Reaped::Untouched => Ok(status),
    }
}

/// What [`reap`] did with a record's entry.
enum Reaped {
    /// Removed it.
    Removed,
    /// Changed nothing on the tailnet.
    Untouched,
    /// It is darkmux's, under an earlier name of this machine, and cannot be
    /// removed: the reason, naming the remedies.
    Blocked(String),
}

/// Deals with the entry `rec` says an earlier `client serve` published,
/// against `status`: removed when the tailnet shows it exactly (the record
/// then goes), left when it changed or is gone (the record goes too), and
/// kept, with its record, when it cannot be read, the removal fails, or it
/// is under a name other than `current`, this machine's name now.
fn reap(rec: &PublishedRecord, status: &[u8], current: &str, rig: &Rig, log: &mut Vec<String>) -> Reaped {
    let path = rig.record(rec.entry.port);
    let url = rec.entry.url();
    match overlay::at_port(status, &rec.entry) {
        Ok(AtPort::Exactly) if current != rec.entry.host => Reaped::Blocked(overlay::renamed_reason(&rec.entry, current)),
        Ok(AtPort::Exactly) => match overlay::remove(&rig.bin, &rec.entry, rig.timeout) {
            Ok(()) => {
                log.push(format!("{LOG} removed the tailnet entry a stopped client serve left ({url})"));
                let _ = std::fs::remove_file(&path);
                Reaped::Removed
            }
            Err(e) => {
                log.push(format!(
                    "{LOG} could not remove the tailnet entry a stopped client serve left ({url}): {e:#}; `{}` removes it",
                    rec.entry.remove_command()
                ));
                Reaped::Untouched
            }
        },
        Ok(AtPort::Other) => {
            log.push(format!("{LOG} the tailnet entry a stopped client serve published ({url}) has changed since; left in place"));
            let _ = std::fs::remove_file(&path);
            Reaped::Untouched
        }
        Ok(AtPort::Free) => {
            let _ = std::fs::remove_file(&path);
            Reaped::Untouched
        }
        Err(e) => {
            log.push(format!("{LOG} could not read the tailnet's serve entries ({e:#}); `{}` removes ({url})", rec.entry.remove_command()));
            Reaped::Untouched
        }
    }
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
    Started { wire: ClientRemote::available(entry.url()), serving: Some(entry.clone()), owned: Some(entry), log }
}

/// After a publish that failed or did not show: an entry that is there
/// exactly is removed ([`stop`]); otherwise the record goes, unless the
/// tailnet could not be read (a publish cut off by its bound may still have
/// landed, so the next start looks again).
fn undo(entry: &ServeEntry, rig: &Rig) -> Vec<String> {
    match overlay::serve_status(&rig.bin, rig.timeout).ok().map(|s| overlay::at_port(&s, entry)) {
        Some(Ok(AtPort::Exactly)) => stop(entry, rig),
        Some(Ok(AtPort::Other)) => {
            let _ = std::fs::remove_file(rig.record(entry.port));
            vec![format!(
                "{LOG} `{} serve` stored something other than darkmux's entry at {}; left in place, since darkmux cannot \
                 tell it from an entry of the operator's. `{}` removes it",
                overlay::TOOL,
                entry.url(),
                entry.remove_command()
            )]
        }
        Some(Ok(AtPort::Free)) => {
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
