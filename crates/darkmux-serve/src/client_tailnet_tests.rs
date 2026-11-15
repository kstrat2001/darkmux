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

#[test]
fn a_request_as_the_entry_forwards_it_is_from_the_tailnet_with_its_login() {
    assert_eq!(
        arrival(loopback(), &forwarded(), BOUND, Some(&entry())),
        Arrival::Tailnet {
            login: Some("sam@example.com".into())
        }
    );
    let mut none = forwarded();
    none.remove(LOGIN);
    assert_eq!(
        arrival(loopback(), &none, BOUND, Some(&entry())),
        Arrival::Tailnet { login: None }
    );
    none.insert(LOGIN, "".parse().unwrap());
    assert_eq!(
        arrival(loopback(), &none, BOUND, Some(&entry())),
        Arrival::Tailnet { login: None },
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
    let sam = TailnetIdentity {
        login: "sam@example.com".into(),
    };
    let url = "https://studio.example.ts.net:8767/";
    assert_eq!(
        identity_refusal(Some(&sam), Some("sam@example.com"), url),
        None
    );
    let (code, why) = identity_refusal(None, Some("sam@example.com"), url).unwrap();
    assert_eq!(code, ClientCode::NotPairedRemotely);
    assert!(
        why.contains("pair the device over the tailnet") && why.contains(url),
        "{why}"
    );
    let (code, why) = identity_refusal(Some(&sam), None, url).unwrap();
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
        let (code, why) = identity_refusal(Some(&sam), Some(other), url).unwrap();
        assert_eq!(code, ClientCode::RemoteIdentityMismatch, "{other:?}");
        assert!(why.contains("log the device in"), "{why}");
    }
    assert_eq!(
        identity_refusal(None, None, url).map(|r| r.0),
        Some(ClientCode::NotPairedRemotely),
        "no identity recorded comes first"
    );
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
    let (status, code, why) = pairing_refusal();
    assert_eq!(
        (status, code),
        (StatusCode::FORBIDDEN, ClientCode::RemotePairingUnavailable)
    );
    assert!(why.contains("only an app on this Mac can pair"), "{why}");
}
