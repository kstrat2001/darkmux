//! Tests for `client_tailnet`: the forwarded-request rule and the identity
//! check, each decision on its own. The HTTP contract (the gate in front of
//! every route) is driven through the router in `client_pairing_tests`.

use super::*;

fn entry() -> ServeEntry {
    ServeEntry::for_port("studio.example.ts.net", 8767)
}

/// What `tailscale serve` sets forwarding a user's request to [`entry`].
fn forwarded() -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in [
        ("host", "studio.example.ts.net:8767"),
        ("x-forwarded-host", "studio.example.ts.net:8767"),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-for", "100.64.1.2"),
        ("tailscale-user-login", "sam@example.com"),
    ] {
        h.insert(k, v.parse().unwrap());
    }
    h
}

fn loopback() -> Option<SocketAddr> {
    Some(SocketAddr::from(([127, 0, 0, 1], 5555)))
}

const BOUND: Option<SocketAddr> = None;

/// The device address [`forwarded`] names.
const PHONE: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(100, 64, 1, 2));

/// Sam's phone as the tailnet's `whois` names it, and as a pairing records it.
fn phone_node() -> darkmux_fleet::NodeIdentity {
    let mut n = darkmux_fleet::test_node("nPHONE1CNTRL", "sams-iphone", "100.64.1.2");
    n.host_name = Some("Sam's iPhone".into());
    n.login = Some("sam@example.com".into());
    n
}

const SAM: &str = "sam@example.com";

fn phone() -> TailnetDevice {
    TailnetDevice { id: "nPHONE1CNTRL".into(), name: "sams-iphone".into(), host_name: Some("Sam's iPhone".into()) }
}

fn sam() -> TailnetIdentity {
    TailnetIdentity { login: "sam@example.com".into(), device: phone() }
}

#[test]
fn a_request_as_the_entry_forwards_it_is_from_the_tailnet_with_its_login() {
    assert_eq!(
        arrival(loopback(), &forwarded(), BOUND, Some(&entry())),
        Arrival::Tailnet {
            login: Some("sam@example.com".into()),
            from: PHONE,
        }
    );
    let mut none = forwarded();
    none.remove(LOGIN);
    assert_eq!(
        arrival(loopback(), &none, BOUND, Some(&entry())),
        Arrival::Tailnet { login: None, from: PHONE }
    );
    none.insert(LOGIN, "".parse().unwrap());
    assert_eq!(
        arrival(loopback(), &none, BOUND, Some(&entry())),
        Arrival::Tailnet { login: None, from: PHONE },
        "an empty login names no one"
    );
}

/// With no entry the same request is foreign; one from this machine is
/// local whatever the entry; one not from loopback is never the entry's.
#[test]
fn the_tailnet_path_needs_the_entry_and_a_loopback_peer() {
    assert_eq!(
        arrival(loopback(), &forwarded(), BOUND, None),
        Arrival::Foreign
    );
    let mut local = HeaderMap::new();
    local.insert("host", "localhost:8767".parse().unwrap());
    assert_eq!(
        arrival(loopback(), &local, BOUND, Some(&entry())),
        Arrival::Local
    );
    assert_eq!(arrival(loopback(), &local, BOUND, None), Arrival::Local);
    let peer = Some(SocketAddr::from(([100, 64, 1, 2], 5555)));
    assert_eq!(
        arrival(peer, &forwarded(), BOUND, Some(&entry())),
        Arrival::Foreign
    );
    assert_eq!(
        arrival(None, &forwarded(), BOUND, Some(&entry())),
        Arrival::Foreign
    );
    let mapped = Some("[::ffff:127.0.0.1]:5555".parse().unwrap());
    assert!(matches!(
        arrival(mapped, &forwarded(), BOUND, Some(&entry())),
        Arrival::Tailnet { .. }
    ));
}

/// Each header the rule requires, missing, repeated or wrong, and each
/// header Tailscale never sets, added: not forwarded by the entry.
#[test]
fn each_part_of_the_forwarded_rule_is_needed() {
    assert!(forwarded_by(&forwarded(), &entry()));
    for name in [
        "host",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-forwarded-for",
    ] {
        let mut h = forwarded();
        h.remove(name);
        assert!(!forwarded_by(&h, &entry()), "without {name}");
        let mut twice = forwarded();
        let v = twice.get(name).unwrap().clone();
        twice.append(name, v);
        assert!(!forwarded_by(&twice, &entry()), "{name} twice");
    }
    for (name, value) in [
        ("host", "studio.example.ts.net:9999"),
        ("host", "studio.example.ts.net"),
        ("x-forwarded-host", "laptop.example.ts.net:8767"),
        ("x-forwarded-proto", "http"),
        ("x-forwarded-for", "192.168.1.9"),
        ("x-forwarded-for", "not-an-address"),
        ("x-forwarded-for", "100.64.1.2, 100.64.0.1"),
    ] {
        let mut h = forwarded();
        h.insert(name, value.parse().unwrap());
        assert!(!forwarded_by(&h, &entry()), "{name}: {value}");
    }
    for name in NOT_TAILSCALE {
        let mut h = forwarded();
        h.insert(*name, "x".parse().unwrap());
        assert!(!forwarded_by(&h, &entry()), "with {name}");
    }
    let mut extra = forwarded();
    for (k, v) in [
        ("tailscale-user-name", "Sam"),
        (
            "tailscale-headers-info",
            "https://tailscale.com/s/serve-headers",
        ),
        ("tailscale-app-capabilities", "{}"),
    ] {
        extra.insert(k, v.parse().unwrap());
    }
    assert!(
        forwarded_by(&extra, &entry()),
        "what Tailscale also sets is allowed"
    );
}

#[test]
fn a_host_names_the_entry_by_name_and_port_and_443_may_go_unsaid() {
    assert!(names_entry("studio.example.ts.net:8767", &entry()));
    assert!(names_entry("STUDIO.example.ts.net:8767", &entry()));
    assert!(!names_entry("studio.example.ts.net", &entry()));
    assert!(!names_entry("studio.example.ts.net.:8767", &entry()));
    assert!(!names_entry("evil.studio.example.ts.net:8767", &entry()));
    let on_443 = ServeEntry::for_port("studio.example.ts.net", 443);
    assert!(names_entry("studio.example.ts.net", &on_443));
    assert!(names_entry("studio.example.ts.net:443", &on_443));
    assert!(!names_entry("studio.example.ts.net:8443", &on_443));
}

#[test]
fn tailnet_addresses_are_tailscales_ranges_only() {
    for ip in [
        "100.64.0.0",
        "100.64.1.2",
        "fd7a:115c:a1e0::1",
        "fd7a:115c:a1e0:ab12::99",
        "::ffff:100.64.0.1",
    ] {
        assert!(is_tailnet_address(ip.parse().unwrap()), "{ip}");
    }
    // The top of 100.64.0.0/10, built rather than written: an address
    // written out inside that range reads as a real one to the repo's
    // sanitization guard.
    let top = std::net::Ipv4Addr::new(100, 64 + 63, 255, 255);
    assert!(is_tailnet_address(top.into()), "{top}");
    for ip in [
        "100.63.255.255",
        "100.128.0.0",
        "10.0.0.1",
        "127.0.0.1",
        "fd7a:115c:a1e1::1",
        "::1",
        "2001:db8::1",
    ] {
        assert!(!is_tailnet_address(ip.parse().unwrap()), "{ip}");
    }
}

/// The token found a client: its recorded identity must be the login
/// Tailscale named, exactly. Each refusal says what to do.
#[test]
fn the_identity_must_be_the_recorded_one_exactly() {
    let sam = sam();
    let url = "https://studio.example.ts.net:8767/";
    assert_eq!(identity_checked(Some(&sam), Some("sam@example.com"), url), Ok(&sam));
    let (code, why) = identity_checked(None, Some("sam@example.com"), url).unwrap_err();
    assert_eq!(code, ClientCode::NotPairedRemotely);
    assert!(
        why.contains("pair the device over the tailnet") && why.contains(url),
        "{why}"
    );
    let (code, why) = identity_checked(Some(&sam), None, url).unwrap_err();
    assert_eq!(code, ClientCode::RemoteIdentityMissing);
    assert!(
        why.contains("tag") && why.contains("log the device in"),
        "{why}"
    );
    for other in [
        "alex@example.com",
        "Sam@example.com",
        "sam@example.com ",
        "",
    ] {
        let (code, why) = identity_checked(Some(&sam), Some(other), url).unwrap_err();
        assert_eq!(code, ClientCode::RemoteIdentityMismatch, "{other:?}");
        assert!(why.contains("log the device in"), "{why}");
    }
    assert_eq!(
        identity_checked(None, None, url).map_err(|r| r.0),
        Err(ClientCode::NotPairedRemotely),
        "no identity recorded comes first"
    );
}

/// (#3560 review) A login that is not ASCII arrives as Tailscale's Go code
/// writes it, `mime.QEncoding.Encode` (`=?utf-8?q?...?=`, `_` for a space,
/// `=XX` for any other byte, words split by single spaces), and reads as the
/// login itself; an ASCII one is the login as sent. Anything malformed names
/// no one.
#[test]
fn a_login_header_reads_as_the_login_it_names() {
    for (header, login) in [
        ("sam@example.com", "sam@example.com"),
        ("=?utf-8?q?j=C3=B6rg@example.com?=", "jörg@example.com"),
        ("=?UTF-8?Q?j=C3=B6rg?=", "jörg"),
        ("=?utf-8?q?a_b=C3=A9?= =?utf-8?q?=E6=97=A5@example.com?=", "a bé日@example.com"),
    ] {
        assert_eq!(decoded_login(header).as_deref(), Some(login), "{header}");
    }
    for bad in [
        "=?utf-8?q?bad=ZZ?=",
        "=?utf-8?q?bad=+1?=",
        "=?utf-8?q?cut=C?=",
        "=?utf-8?q?=FF=FE?=",
        "=?iso-8859-1?q?j=F6rg?=",
        "=?utf-8?b?am9yZw==?=",
        "=?iso-8859-1?q?jorg?=",
        "=?utf-8?b?jorg?=",
        "=?utf-8?q?unclosed",
        "=?utf-8?q??=",
        "=?utf-8?q?a?=  =?utf-8?q?b?=",
    ] {
        assert_eq!(decoded_login(bad), None, "{bad}");
    }
}

/// The login is decoded once, as the request arrives: everything after it
/// compares the login itself.
#[test]
fn a_request_arrives_with_its_login_decoded() {
    let mut h = forwarded();
    h.insert(LOGIN, "=?utf-8?q?j=C3=B6rg@example.com?=".parse().unwrap());
    assert_eq!(arrival(loopback(), &h, BOUND, Some(&entry())), Arrival::Tailnet { login: Some("jörg@example.com".into()), from: PHONE });
    h.insert(LOGIN, "=?utf-8?q?bad=ZZ?=".parse().unwrap());
    assert_eq!(arrival(loopback(), &h, BOUND, Some(&entry())), Arrival::Tailnet { login: None, from: PHONE }, "malformed is missing");
}

/// The status is kept for its time to live, and not past it.
#[test]
fn the_status_is_kept_for_its_time_to_live() {
    let cache = StatusCache::new(std::time::Duration::from_secs(10));
    let t0 = std::time::Instant::now();
    assert_eq!(cache.get(t0), None);
    let status = TailnetStatus { own_login: Some(SAM.into()), peers: vec![PHONE] };
    cache.put(status.clone(), t0);
    assert_eq!(cache.get(t0 + std::time::Duration::from_secs(9)), Some(status));
    assert_eq!(cache.get(t0 + std::time::Duration::from_secs(10)), None);
}

/// A pairing route through the entry needs a tailnet user to record; with
/// none (a tagged device) it is refused, naming the fix.
#[test]
fn a_pairing_needs_a_tailnet_user() {
    assert_eq!(pairing_login(Some("sam@example.com")), Ok("sam@example.com".to_string()));
    let (code, why) = pairing_login(None).unwrap_err();
    assert_eq!(code, ClientCode::RemoteIdentityMissing);
    assert!(why.contains("tag") && why.contains("pair"), "{why}");
}

/// The device `whois` names at the forwarded address: its stable id, name
/// and host name, or, fail closed, a refusal when it named none or failed.
#[test]
fn whois_names_the_device_or_the_request_is_refused() {
    assert_eq!(device_named(Ok(Some(phone_node())), SAM), Ok(phone()));
    for found in [Ok(None), Err("tailscaled is not running".to_string())] {
        let (code, why) = device_named(found.clone(), SAM).unwrap_err();
        assert_eq!(code, ClientCode::RemoteDeviceUnknown, "{found:?}");
        assert!(why.contains("tailscale status") && !why.contains("tailscaled is not running"), "{why}");
    }
}

/// (#3560 review) The header's user must be the one Tailscale's own record
/// gives the device at that address (`UserProfile.LoginName`), exactly: a
/// local process can send any header and any address, but not change what
/// `whois` says. A device whose record names no user is refused too.
#[test]
fn whois_must_confirm_the_headers_user() {
    for header in ["alex@example.com", "Sam@example.com", "sam@example.com "] {
        let (code, why) = device_named(Ok(Some(phone_node())), header).unwrap_err();
        assert_eq!(code, ClientCode::RemoteIdentityUnconfirmed, "{header:?}");
        assert!(why.contains("sam@example.com"), "names whose device it is: {why}");
    }
    let mut tagged = phone_node();
    tagged.login = None;
    assert_eq!(device_named(Ok(Some(tagged.clone())), SAM).map_err(|r| r.0), Err(ClientCode::RemoteIdentityUnconfirmed));
    assert_eq!(device_refusal(&phone(), Ok(Some(tagged)), SAM).map(|r| r.0), Some(ClientCode::RemoteIdentityUnconfirmed));
    assert_eq!(device_refusal(&phone(), Ok(Some(phone_node())), "alex@example.com").map(|r| r.0), Some(ClientCode::RemoteIdentityUnconfirmed));
}

/// (#3560 review) A pairing over the tailnet is open only to the tailnet
/// user this Mac is logged in as; another is refused naming who may, and
/// when Tailscale cannot say who that is, the pairing is refused for now.
#[test]
fn only_this_macs_own_tailnet_user_may_pair() {
    assert_eq!(owner_refusal(SAM, &Ok(Some(SAM.to_string()))), None);
    let (status, code, why) = owner_refusal("alex@example.com", &Ok(Some(SAM.to_string()))).unwrap();
    assert_eq!((status, code), (StatusCode::FORBIDDEN, ClientCode::RemoteUserNotOwner));
    assert!(why.contains(SAM) && why.contains("log the device in"), "{why}");
    for own in [Ok(None), Err("tailscaled is not running".to_string())] {
        let (status, code, why) = owner_refusal(SAM, &own).unwrap();
        assert_eq!((status, code), (StatusCode::SERVICE_UNAVAILABLE, ClientCode::Unavailable), "{own:?}");
        assert!(why.contains("tailscale status") && !why.contains("not running"), "{why}");
    }
}

/// A token's pairing pins its device: only the same stable id passes. The
/// name is the device's own to change, so it never decides.
#[test]
fn a_device_must_be_the_one_the_token_paired_from() {
    assert_eq!(device_refusal(&phone(), Ok(Some(phone_node())), SAM), None);
    let mut renamed = phone_node();
    renamed.name = "sams-new-name".into();
    assert_eq!(device_refusal(&phone(), Ok(Some(renamed)), SAM), None, "a rename is the same device");
    let mut laptop = darkmux_fleet::test_node("nLAPTOP1CNTRL", "sams-iphone", "100.64.1.9");
    laptop.login = Some(SAM.into());
    let (code, why) = device_refusal(&phone(), Ok(Some(laptop)), SAM).unwrap();
    assert_eq!(code, ClientCode::RemoteDeviceMismatch);
    assert!(why.contains("sams-iphone") && why.contains("pair"), "{why}");
    for found in [Ok(None), Err("timed out".to_string())] {
        assert_eq!(device_refusal(&phone(), found, SAM).map(|r| r.0), Some(ClientCode::RemoteDeviceUnknown));
    }
}

/// A pairing is collected the way it was asked for: from this Mac, or
/// through the entry by the same user on the same device.
#[test]
fn a_poll_must_come_the_way_its_pairing_did() {
    let sam = sam();
    assert_eq!(poll_refusal(None, None), None);
    assert_eq!(poll_refusal(Some(&sam), Some(&sam)), None);
    assert_eq!(poll_refusal(Some(&sam), None), Some(PollRefusal::NotFound), "from this Mac");
    let refused = |r: Option<PollRefusal>| match r {
        Some(PollRefusal::Coded(code, _)) => Some(code),
        _ => None,
    };
    assert_eq!(refused(poll_refusal(None, Some(&sam))), Some(ClientCode::NotPairedRemotely));
    let alex = TailnetIdentity { login: "alex@example.com".into(), ..sam.clone() };
    assert_eq!(refused(poll_refusal(Some(&sam), Some(&alex))), Some(ClientCode::RemoteIdentityMismatch));
    let mut laptop = sam.clone();
    laptop.device.id = "nLAPTOP1CNTRL".into();
    assert_eq!(refused(poll_refusal(Some(&sam), Some(&laptop))), Some(ClientCode::RemoteDeviceMismatch));
}

/// What the operator is shown about a remote pairing: Tailscale's user and
/// device, never with a character that could disguise them.
#[test]
fn a_remote_identity_is_described_safely() {
    assert_eq!(described(&sam()), "tailnet user sam@example.com, device sams-iphone (Sam's iPhone)");
    let mut sly = sam();
    sly.login = "sam@example.com\u{202E}moc".into();
    sly.device.host_name = None;
    let shown = described(&sly);
    assert!(!shown.contains('\u{202E}') && shown.ends_with("device sams-iphone"), "{shown}");
}

/// (#3626) A paired device chooses its own names, so each one is cut to
/// [`NAME_COLUMNS`] display columns (not bytes, not chars) at a visible `…`;
/// one that fits exactly is shown whole.
#[test]
fn a_tailnet_name_is_bounded_by_display_columns_with_a_visible_cut() {
    use darkmux_flow::hooks::display_columns;
    let at_limit = [
        "a".repeat(NAME_COLUMNS),
        "長".repeat(NAME_COLUMNS / 2),
        format!("{}é", "e\u{301}".repeat(NAME_COLUMNS - 1)),
    ];
    for name in &at_limit {
        assert_eq!(&shown_name(name), name, "exactly {NAME_COLUMNS} columns is shown whole");
    }
    assert_eq!(shown_name("Sam's\niPhone\u{202E}"), "Sam's iPhone", "a line break still separates words");
    let over = [
        ("one column over", "a".repeat(NAME_COLUMNS + 1)),
        ("one wide char over", format!("a{}", "長".repeat(NAME_COLUMNS / 2))),
        ("full-width CJK", "長".repeat(200)),
        ("combining marks", "e\u{301}".repeat(200)),
        ("a stack of combining marks", format!("a{}", "\u{301}".repeat(1000))),
        ("emoji", "😀".repeat(100)),
        ("emoji presentation selectors", "☺\u{FE0F}".repeat(100)),
        ("flags", "🇺🇸".repeat(30)),
        ("skin tones", "👋🏽".repeat(30)),
        ("one heavily marked last character", format!("{}{}", "a".repeat(NAME_COLUMNS), "\u{301}".repeat(200))),
    ];
    for (what, name) in &over {
        let shown = shown_name(name);
        let columns = display_columns(&shown);
        assert!(columns <= NAME_COLUMNS && shown.contains('…'), "{what}: {columns} columns: {shown}");
        assert!(shown.chars().count() <= 4 * NAME_COLUMNS, "{what}: {} chars", shown.chars().count());
        // U+FE0F turns a 1-column character into a 2-column emoji, which a
        // per-character column count does not see: none may survive.
        assert!(!shown.contains('\u{FE0F}'), "{what}: {shown}");
        // The end shows too: the name's last character that takes a column
        // is after the cut, unless it is the name's only one.
        let (_, end) = shown.split_once('…').expect("cut");
        let mut bases = name.chars().filter(|c| display_columns(&c.to_string()) > 0);
        let last = bases.clone().next_back().expect("a base");
        assert!(end.contains(last) || bases.nth(1).is_none(), "{what}: {shown}");
    }
}

/// (#3626) `described` shows all three names bounded: however long a
/// device names itself, the identity takes its own words plus at most three
/// names' columns.
#[test]
fn a_described_identity_is_bounded_whatever_the_device_names_itself() {
    use darkmux_flow::hooks::display_columns;
    let blank = TailnetDevice { id: "n".into(), name: String::new(), host_name: Some(String::new()) };
    let words = display_columns(&described(&TailnetIdentity { login: String::new(), device: blank }));
    let mut long = sam();
    long.login = format!("{}@example.com", "長".repeat(200));
    long.device.name = "長".repeat(200);
    long.device.host_name = Some("e\u{301}".repeat(500));
    let shown = described(&long);
    assert!(display_columns(&shown) <= words + 3 * NAME_COLUMNS, "{} columns: {shown}", display_columns(&shown));
    assert_eq!(shown.matches('…').count(), 3, "every name is cut visibly: {shown}");
    assert!(shown.starts_with("tailnet user 長") && shown.contains(", device 長"), "{shown}");
}

#[test]
fn a_request_from_neither_is_not_local_with_no_entry_and_not_forwarded_with_one() {
    let (status, code, _) = not_from_here(None);
    assert_eq!(
        (status, code),
        (StatusCode::FORBIDDEN, ClientCode::NotLocal)
    );
    let (status, code, why) = not_from_here(Some(&entry()));
    assert_eq!(
        (status, code),
        (StatusCode::FORBIDDEN, ClientCode::NotForwarded)
    );
    assert!(
        why.contains("https://studio.example.ts.net:8767/")
            && why.contains("studio.example.ts.net:8767"),
        "{why}"
    );
}
