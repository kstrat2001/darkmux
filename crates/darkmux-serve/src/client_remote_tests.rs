//! (#3560) `client serve`'s tailnet lifecycle against a fake `tailscale`:
//! a shell script that records every argument list and keeps a serve
//! configuration in a file, so publish, remove and status behave like the
//! tool's. Nothing here reaches the real tool or any tailnet.

use super::*;
use std::path::Path;

const PORT: u16 = 8767;
const HOST: &str = "studio.example.ts.net";

/// The operator's own serve entries, which no test may ever see changed:
/// the viewer on :80 and something on :8443.
const OPERATOR: &str = r#"{"TCP":{"80":{"HTTP":true},"8443":{"HTTPS":true}},
  "Web":{"studio.example.ts.net:80":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8765"}}},
         "studio.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"}}}}}"#;

const RUNNING: &str = r#"{"BackendState":"Running","Self":{"DNSName":"studio.example.ts.net."},"CertDomains":["studio.example.ts.net"]}"#;

/// The operator's entries plus `extra` merged into `TCP` and `Web`.
fn with(extra_tcp: serde_json::Value, extra_web: serde_json::Value) -> String {
    let mut v: serde_json::Value = serde_json::from_str(OPERATOR).unwrap();
    for (k, x) in extra_tcp.as_object().unwrap() {
        v["TCP"][k] = x.clone();
    }
    for (k, x) in extra_web.as_object().unwrap() {
        v["Web"][k] = x.clone();
    }
    v.to_string()
}

/// The operator's entries plus darkmux's for `PORT`, exactly as published.
fn with_ours() -> String {
    with(
        serde_json::json!({ "8767": { "HTTPS": true } }),
        serde_json::json!({ "studio.example.ts.net:8767": { "Handlers": { "/": { "Proxy": "http://127.0.0.1:8767" } } } }),
    )
}

/// The fake `tailscale` (`tests/fixtures/fake_tailscale.py`), whose `serve`
/// changes follow Tailscale 1.102.4's own code, in a temp dir of its own.
const FAKE_TOOL: &str = include_str!("../tests/fixtures/fake_tailscale.py");

/// One fake tool and its state.
struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new(status: &str, serve: &str) -> Fake {
        let dir = tempfile::TempDir::new().unwrap();
        let d = dir.path();
        std::fs::write(d.join("status.json"), status).unwrap();
        std::fs::write(d.join("serve.json"), serve).unwrap();
        let bin = d.join("tailscale");
        std::fs::write(&bin, FAKE_TOOL).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = d;
        let f = Fake { dir };
        let d = f.dir.path();
        std::fs::write(d.join("record_path"), f.rig().record(PORT).to_string_lossy().as_bytes()).unwrap();
        f
    }

    /// Writes switch `name` (see the fixture's doc) with `body`.
    fn switch(&self, name: &str, body: &str) {
        std::fs::write(self.dir.path().join(name), body).unwrap();
    }

    fn rig(&self) -> Rig {
        Rig {
            bin: self.dir.path().join("tailscale").to_string_lossy().into_owned(),
            records: self.dir.path().join("records"),
            // Long: macOS scans a freshly written executable on its first run.
            timeout: Duration::from_secs(60),
            publish_timeout: Duration::from_secs(60),
            pid: 4242,
        }
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("args.log")).unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// The calls that change the serve configuration.
    fn changes(&self) -> Vec<String> {
        self.calls().into_iter().filter(|c| c.starts_with("serve ") && !c.starts_with("serve status")).collect()
    }

    fn serve(&self) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(self.dir.path().join("serve.json")).unwrap()).unwrap()
    }

    fn record(&self) -> Option<PublishedRecord> {
        overlay::read_record(&self.rig().record(PORT))
    }

    fn plant_record(&self, entry: ServeEntry) {
        overlay::write_record(&self.rig().record(PORT), &PublishedRecord { entry, pid: 1 }).unwrap();
    }
}

fn operator() -> serde_json::Value {
    serde_json::from_str(OPERATOR).unwrap()
}

fn ours() -> ServeEntry {
    ServeEntry::for_port(HOST, PORT)
}

/// Off runs nothing: the tool is never called, and whoami says off.
#[test]
fn off_never_runs_the_tool() {
    let f = Fake::new(RUNNING, OPERATOR);
    let s = start(ClientsRemote::Off, PORT, &f.rig());
    assert_eq!(s, Started { wire: ClientRemote::off(), owned: None, log: vec![] });
    assert!(f.calls().is_empty(), "{:?}", f.calls());
}

/// (#3560 review) Off with a record left from an earlier run: one status
/// read, one removal of exactly the entry the record proves, and the record
/// goes. Off never publishes.
#[test]
fn off_removes_only_the_entry_its_record_proves_it_left() {
    let f = Fake::new(RUNNING, &with_ours());
    f.plant_record(ours());
    let s = start(ClientsRemote::Off, PORT, &f.rig());
    assert_eq!(s.wire, ClientRemote::off());
    assert_eq!(s.owned, None);
    assert_eq!(f.calls(), ["serve status --json", "serve --https=8767 --set-path=/ off"]);
    assert_eq!(f.serve(), operator(), "only darkmux's entry went");
    assert_eq!(f.record(), None);
    assert!(s.log.iter().any(|l| l.contains("removed the tailnet entry") && l.contains("clients.remote is off")), "{:?}", s.log);
}

/// (#3560 review) Off with a record whose entry the operator has since
/// changed: one status read, nothing removed, the record dropped.
#[test]
fn off_leaves_an_entry_that_changed_since_its_record() {
    let changed = with(
        serde_json::json!({ "8767": { "HTTPS": true } }),
        serde_json::json!({ "studio.example.ts.net:8767": { "Handlers": {
            "/": { "Proxy": "http://127.0.0.1:8767" }, "/api": { "Proxy": "http://127.0.0.1:9000" } } } }),
    );
    let f = Fake::new(RUNNING, &changed);
    f.plant_record(ours());
    let s = start(ClientsRemote::Off, PORT, &f.rig());
    assert_eq!(f.calls(), ["serve status --json"], "{:?}", s.log);
    assert_eq!(f.record(), None);
}

/// On and usable: the record is written, the entry published and checked,
/// and stop removes exactly it, leaving the operator's entries as they were.
#[test]
fn on_publishes_then_stop_removes_only_its_own_entry() {
    let f = Fake::new(RUNNING, OPERATOR);
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    assert_eq!(s.wire, ClientRemote::available("https://studio.example.ts.net:8767/".into()), "{s:?}");
    assert_eq!(s.owned, Some(ours()));
    assert!(s.log.iter().any(|l| l.contains("on the tailnet at https://studio.example.ts.net:8767/") && l.contains("refused")), "{:?}", s.log);
    assert_eq!(f.record(), Some(PublishedRecord { entry: ours(), pid: 4242 }));
    assert_eq!(f.changes(), ["serve --bg --https=8767 http://127.0.0.1:8767"]);
    assert_eq!(f.calls()[..2], ["version".to_string(), "status --json".to_string()], "read-only calls first");

    let lines = stop(&ours(), &f.rig());
    assert_eq!(lines, ["darkmux client serve: removed its tailnet entry (https://studio.example.ts.net:8767/)"]);
    assert_eq!(f.changes(), ["serve --bg --https=8767 http://127.0.0.1:8767", "serve --https=8767 --set-path=/ off"]);
    assert_eq!(f.serve(), operator(), "the operator's entries are untouched");
    assert_eq!(f.record(), None, "the record goes with the entry");
}

/// Each way the network cannot carry the entry: unavailable with the reason
/// and its fix, and nothing on the tailnet changed.
#[test]
fn on_but_unusable_serves_locally_with_one_reason() {
    for (status, why) in [
        (r#"{"BackendState":"NeedsLogin","Self":{"DNSName":""}}"#, "not logged in"),
        (r#"{"BackendState":"Stopped","Self":{"DNSName":"studio.example.ts.net."}}"#, "not running"),
        (r#"{"BackendState":"Running","Self":{"DNSName":"studio.example.ts.net."},"CertDomains":null}"#, "HTTPS certificates are off"),
    ] {
        let f = Fake::new(status, OPERATOR);
        let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
        assert_eq!(s.wire.state, crate::client_wire::RemoteState::Unavailable, "{why}: {s:?}");
        let reason = s.wire.reason.clone().unwrap();
        assert!(reason.contains(why) && reason.contains('('), "{why}: the reason names the fix: {reason}");
        assert_eq!(s.owned, None);
        assert_eq!(s.log.len(), 1, "one clear line: {:?}", s.log);
        assert!(s.log[0].contains(&reason) && s.log[0].contains("serving this machine only"), "{:?}", s.log);
        assert!(f.changes().is_empty() && f.record().is_none(), "{why}: {:?}", f.calls());
    }
    let missing = Rig { bin: "/nonexistent/darkmux-test-no-such-tool".into(), ..Fake::new(RUNNING, OPERATOR).rig() };
    let s = start(ClientsRemote::Tailscale, PORT, &missing);
    assert!(s.wire.reason.as_deref().is_some_and(|r| r.contains("not installed")), "{s:?}");
}

/// The operator's own entry on the port (no record of darkmux's) is never
/// replaced or removed: darkmux serves this machine only and says why.
#[test]
fn an_operators_entry_on_the_port_is_left_alone() {
    let theirs = with(
        serde_json::json!({ "8767": { "HTTPS": true } }),
        serde_json::json!({ "studio.example.ts.net:8767": { "Handlers": { "/": { "Proxy": "http://127.0.0.1:5173" } } } }),
    );
    let f = Fake::new(RUNNING, &theirs);
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    assert!(s.wire.reason.as_deref().is_some_and(|r| r.contains("already served by another tailnet entry")), "{s:?}");
    assert!(f.changes().is_empty(), "{:?}", f.calls());
    assert_eq!(f.serve(), serde_json::from_str::<serde_json::Value>(&theirs).unwrap());
    assert_eq!(f.record(), None);
}

/// An entry identical to darkmux's but with no record is used, never owned:
/// shutdown leaves it.
#[test]
fn an_identical_entry_with_no_record_is_used_but_never_removed() {
    let f = Fake::new(RUNNING, &with_ours());
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    assert_eq!(s.wire, ClientRemote::available(ours().url()));
    assert_eq!(s.owned, None, "not darkmux's to remove");
    assert!(f.changes().is_empty() && f.record().is_none(), "{:?}", f.calls());
}

/// A record and the exact entry left by a killed `client serve`: removed,
/// then published again, and the operator's entries untouched throughout.
#[test]
fn a_killed_serves_entry_is_reaped_then_published_again() {
    let f = Fake::new(RUNNING, &with_ours());
    f.plant_record(ours());
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    assert_eq!(s.owned, Some(ours()), "{s:?}");
    assert!(s.log.iter().any(|l| l.contains("removed the tailnet entry a stopped client serve left")), "{:?}", s.log);
    assert_eq!(f.changes(), ["serve --https=8767 --set-path=/ off", "serve --bg --https=8767 http://127.0.0.1:8767"]);
    assert_eq!(f.record(), Some(PublishedRecord { entry: ours(), pid: 4242 }), "the new record");
}

/// A record whose entry the operator has since changed: the entry is left,
/// the record dropped, and the port is someone else's now.
#[test]
fn a_record_whose_entry_changed_is_dropped_and_the_entry_left() {
    let changed = with(
        serde_json::json!({ "8767": { "HTTPS": true } }),
        serde_json::json!({ "studio.example.ts.net:8767": { "Handlers": {
            "/": { "Proxy": "http://127.0.0.1:8767" }, "/api": { "Proxy": "http://127.0.0.1:9000" } } } }),
    );
    let f = Fake::new(RUNNING, &changed);
    f.plant_record(ours());
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    assert!(s.log.iter().any(|l| l.contains("has changed since; left in place")), "{:?}", s.log);
    assert_eq!(s.wire.state, crate::client_wire::RemoteState::Unavailable);
    assert!(f.changes().is_empty(), "{:?}", f.calls());
    assert_eq!(f.record(), None);
}

/// (#3560 review) A record for a name this machine no longer has (renamed
/// on the tailnet): `tailscale serve` removes only under the current name,
/// so darkmux runs no removal it knows cannot work, serves this machine
/// only with the reason and a remedy that works, keeps the record, and the
/// next start says the same without trying again. Once the entry is gone
/// the record goes and the entry is published again.
#[test]
fn a_record_under_an_old_name_is_reported_not_retried() {
    let old = ServeEntry::for_port("mini.example.ts.net", PORT);
    let left = with(
        serde_json::json!({ "8767": { "HTTPS": true } }),
        serde_json::json!({ "mini.example.ts.net:8767": { "Handlers": { "/": { "Proxy": "http://127.0.0.1:8767" } } } }),
    );
    let f = Fake::new(RUNNING, &left);
    f.plant_record(old.clone());
    for _ in 0..2 {
        let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
        let reason = s.wire.reason.clone().unwrap_or_default();
        assert!(reason.contains("renamed") && reason.contains("mini.example.ts.net") && reason.contains("tailscale serve reset"), "{reason}");
        assert_eq!(s.owned, None);
    }
    assert!(f.changes().is_empty(), "no removal that cannot work: {:?}", f.calls());
    assert_eq!(f.record().map(|r| r.entry), Some(old.clone()));
    // The remedy really works in the fake: the CLI removes only under the
    // current name, so a direct removal fails there.
    assert!(overlay::remove(&f.rig().bin, &old, f.rig().timeout).is_err());
    std::fs::write(f.dir.path().join("serve.json"), OPERATOR).unwrap();
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    assert_eq!(s.owned, Some(ours()), "{s:?}");
}

/// (#3560 review) The removal names its mount, `/`: an operator's mount
/// added on the port between darkmux's read and its removal survives.
/// Plain `off` would take every mount on the port.
#[test]
fn removal_takes_only_darkmuxs_mount_when_the_operator_adds_one_meanwhile() {
    let f = Fake::new(RUNNING, OPERATOR);
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    f.switch("race_mount.json", r#"{"/api": {"Proxy": "http://127.0.0.1:9000"}}"#);
    let lines = stop(&s.owned.unwrap(), &f.rig());
    assert!(lines[0].contains("removed its tailnet entry"), "{lines:?}");
    assert_eq!(
        f.serve()["Web"]["studio.example.ts.net:8767"]["Handlers"],
        serde_json::json!({ "/api": { "Proxy": "http://127.0.0.1:9000" } }),
        "the operator's mount survives"
    );
    assert_eq!(f.serve()["TCP"]["8767"], serde_json::json!({ "HTTPS": true }));
}

/// (#3560 review) A publish that fails and leaves the serve configuration
/// unreadable: the record stays, so the next start looks again (the publish
/// may have landed).
#[test]
fn a_failed_publish_whose_aftermath_cannot_be_read_keeps_the_record() {
    let f = Fake::new(RUNNING, OPERATOR);
    f.switch("break_on_publish", "");
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    assert_eq!(s.owned, None, "{s:?}");
    assert_eq!(f.record().map(|r| r.pid), Some(4242), "kept");
}

/// A publish the tool refuses: unavailable naming its reason, and no record
/// left behind.
#[test]
fn a_refused_publish_leaves_no_record() {
    let f = Fake::new(RUNNING, OPERATOR);
    f.switch("refuse", "");
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    assert!(s.wire.reason.as_deref().is_some_and(|r| r.contains("Access denied")), "{s:?}");
    assert_eq!(s.owned, None);
    assert_eq!(f.record(), None);
    assert_eq!(f.serve(), operator());
}

/// A stop whose entry was changed by the operator meanwhile leaves it, and
/// one whose removal fails keeps the record for the next start.
#[test]
fn stop_leaves_a_changed_entry_and_keeps_the_record_when_removal_fails() {
    let f = Fake::new(RUNNING, OPERATOR);
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    f.switch("refuse", "");
    let lines = stop(&s.owned.clone().unwrap(), &f.rig());
    assert!(lines[0].contains("could not remove") && lines[0].contains("tailscale serve --https=8767 --set-path=/ off"), "{lines:?}");
    assert!(f.record().is_some(), "kept, so the next start removes it");

    let changed = Fake::new(RUNNING, OPERATOR);
    let s = start(ClientsRemote::Tailscale, PORT, &changed.rig());
    std::fs::write(changed.dir.path().join("serve.json"), OPERATOR).unwrap();
    let lines = stop(&s.owned.unwrap(), &changed.rig());
    assert_eq!(lines, ["darkmux client serve: its tailnet entry was already gone"]);
    assert_eq!(changed.changes(), ["serve --bg --https=8767 http://127.0.0.1:8767"], "nothing removed");
    assert_eq!(changed.record(), None);
}

/// The fake really is what the tests ran: the records live in its own
/// temp dir, never this machine's darkmux root.
#[test]
fn the_fake_rig_keeps_its_records_in_its_own_dir() {
    let f = Fake::new(RUNNING, OPERATOR);
    assert!(f.rig().records.starts_with(f.dir.path()));
    assert!(Path::new(&f.rig().bin).starts_with(f.dir.path()));
}

/// A tool whose `serve status` fails: a start serves this machine only,
/// naming the tool's reason, and changes nothing; a stop keeps its record
/// so the next start removes the entry.
#[test]
fn an_unreadable_serve_status_changes_nothing_and_keeps_the_record() {
    let f = Fake::new(RUNNING, OPERATOR);
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    assert!(s.owned.is_some());
    std::fs::write(f.dir.path().join("serve.json"), "not json").unwrap();
    let lines = stop(&ours(), &f.rig());
    assert!(lines[0].contains("could not read the tailnet's serve entries"), "{lines:?}");
    assert!(f.record().is_some(), "kept for the next start");

    let broken = Fake::new(RUNNING, "not json");
    let s = start(ClientsRemote::Tailscale, PORT, &broken.rig());
    assert!(s.wire.reason.as_deref().is_some_and(|r| r.contains("not JSON")), "{s:?}");
    assert!(broken.changes().is_empty());

    let failing = Fake::new(RUNNING, OPERATOR);
    failing.switch("status_fails", "");
    let s = start(ClientsRemote::Tailscale, PORT, &failing.rig());
    assert!(s.wire.reason.as_deref().is_some_and(|r| r.contains("permission denied")), "{s:?}");
    let lines = stop(&ours(), &failing.rig());
    assert!(lines[0].contains("could not read") && lines[0].contains("permission denied"), "{lines:?}");
}

/// A record whose entry is already gone is dropped and the entry published
/// afresh; one whose removal fails is kept (the next start tries again),
/// and the entry that is still there is used, not owned.
#[test]
fn a_stale_record_with_nothing_left_or_a_failed_reap() {
    let f = Fake::new(RUNNING, OPERATOR);
    f.plant_record(ours());
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    assert_eq!(f.changes(), ["serve --bg --https=8767 http://127.0.0.1:8767"], "nothing to reap: {s:?}");
    assert_eq!(f.record().map(|r| r.pid), Some(4242), "the new record");

    let stuck = Fake::new(RUNNING, &with_ours());
    stuck.plant_record(ours());
    stuck.switch("refuse", "");
    let s = start(ClientsRemote::Tailscale, PORT, &stuck.rig());
    assert!(s.log.iter().any(|l| l.contains("could not remove the tailnet entry a stopped client serve left")), "{:?}", s.log);
    assert_eq!(s.wire, ClientRemote::available(ours().url()));
    assert_eq!(s.owned, None);
    assert_eq!(stuck.record().map(|r| r.pid), Some(1), "the old record stays for the next start");
}

/// A stop whose entry the operator has since changed (another handler on
/// it) leaves it in place and drops the record.
#[test]
fn stop_leaves_an_entry_the_operator_changed() {
    let f = Fake::new(RUNNING, OPERATOR);
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    let changed = with(
        serde_json::json!({ "8767": { "HTTPS": true } }),
        serde_json::json!({ "studio.example.ts.net:8767": { "Handlers": {
            "/": { "Proxy": "http://127.0.0.1:8767" }, "/api": { "Proxy": "http://127.0.0.1:9000" } } } }),
    );
    std::fs::write(f.dir.path().join("serve.json"), &changed).unwrap();
    let lines = stop(&s.owned.unwrap(), &f.rig());
    assert!(lines[0].contains("changed since it was published; left in place"), "{lines:?}");
    assert_eq!(f.changes(), ["serve --bg --https=8767 http://127.0.0.1:8767"], "nothing removed");
    assert_eq!(f.serve(), serde_json::from_str::<serde_json::Value>(&changed).unwrap());
    assert_eq!(f.record(), None);
}

/// A Tailscale that accepts the publish but files the entry in another
/// shape (one that reads the arguments differently): not darkmux's to
/// claim, so serving this machine only, the record dropped, and the
/// reason naming what darkmux ran.
#[test]
fn a_publish_the_tailnet_does_not_show_exactly_is_not_claimed() {
    let f = Fake::new(RUNNING, OPERATOR);
    f.switch("publish_as.json", r#"{"Proxy": "http://localhost:8767"}"#);
    let s = start(ClientsRemote::Tailscale, PORT, &f.rig());
    assert_eq!(s.owned, None, "{s:?}");
    let reason = s.wire.reason.clone().unwrap_or_default();
    assert!(reason.contains("did not show the entry") && reason.contains("serve --bg --https=8767"), "{reason}");
    assert_eq!(f.changes(), ["serve --bg --https=8767 http://127.0.0.1:8767"], "what it cannot claim it does not remove");
    assert_eq!(f.record(), None);
    // (#3560 review) Left up, but said: the entry and the exact command.
    assert!(
        s.log.iter().any(|l| l.contains("left in place") && l.contains("tailscale serve --https=8767 --set-path=/ off")),
        "{:?}",
        s.log
    );
}
