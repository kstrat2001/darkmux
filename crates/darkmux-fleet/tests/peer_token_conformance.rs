//! (#2916 re-review MUST 3, round 3 C4) The fleet token leaves this machine
//! through ONE helper, `darkmux_fleet::peer` (`fleet_get` /
//! `fleet_post_json`), which only attaches it to a target pinned to an
//! address the identity provider verified.
//!
//! **This is a tripwire, not a proof.** It reads the non-test source of the
//! workspace, `runtime/` and `plugins/` as TEXT and fails when something
//! outside the allowlist does one of the ordinary things that would put the
//! token on the wire: reads it (`serve_token()`, the env var, the Keychain
//! item), or attaches an `Authorization` header (`.set(` / `.header(` with
//! that name in any case, or `bearer_auth`). Code that builds the header
//! name at run time, or reads the token through some other path, passes it.
//! Review is still the gate; this catches the easy regression.
//!
//! Reading the token is allowed where it is COMPARED, never sent: the flow
//! crate's own definition, the viewer's request gate, and the fleet
//! listener's gate.

use std::path::{Path, PathBuf};

const TOKEN_READ_ALLOWED: &[&str] = &[
    "crates/darkmux-flow/src/lib.rs",
    "crates/darkmux-serve/src/lib.rs",
    "crates/darkmux-serve/src/fleet_listener.rs",
    "crates/darkmux-fleet/src/peer.rs",
];
const HEADER_SET_ALLOWED: &[&str] = &["crates/darkmux-fleet/src/peer.rs"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "tests" || n == "target" || n == "node_modules") {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The file's non-test part: everything before its first `#[cfg(test)]`
/// MODULE (`#[cfg(test)]` followed by `mod`), so a test-only helper
/// function earlier in the file does not hide the production code after it.
fn production_part(src: &str) -> &str {
    let mut from = 0;
    while let Some(i) = src[from..].find("#[cfg(test)]") {
        let at = from + i;
        let rest = src[at + "#[cfg(test)]".len()..].trim_start();
        if rest.starts_with("mod ") || rest.starts_with("pub mod ") || rest.starts_with("pub(crate) mod ") {
            return &src[..at];
        }
        from = at + 1;
    }
    src
}

/// The offending patterns in `prod`: (reads the token, attaches a header).
fn findings(prod: &str) -> (bool, bool) {
    let lower = prod.to_ascii_lowercase();
    let reads = prod.contains("serve_token()")
        || prod.contains("var(\"DARKMUX_SERVE_TOKEN\")")
        || prod.contains("var_os(\"DARKMUX_SERVE_TOKEN\")")
        || prod.contains("secret(\"darkmux-serve-token\")")
        || prod.contains(".arg(\"darkmux-serve-token\")");
    let sets = lower.contains(".set(\"authorization\"")
        || lower.contains(".header(\"authorization\"")
        || lower.contains("bearer_auth");
    (reads, sets)
}

#[test]
fn the_fleet_token_is_attached_only_by_the_peer_helper() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("runtime"), &mut files);
    rust_files(&root.join("plugins"), &mut files);
    for c in std::fs::read_dir(root.join("crates")).unwrap().flatten() {
        rust_files(&c.path().join("src"), &mut files);
    }
    assert!(files.len() > 50, "the scan found too few files ({}); its roots moved", files.len());
    let mut offenders = Vec::new();
    for f in files {
        let rel = f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        if name.ends_with("_tests.rs") || name == "tests.rs" || rel.contains("/testing/") {
            continue;
        }
        let src = std::fs::read_to_string(&f).unwrap();
        let (reads, sets) = findings(production_part(&src));
        if reads && !TOKEN_READ_ALLOWED.contains(&rel.as_str()) {
            offenders.push(format!("{rel}: reads the fleet token"));
        }
        if sets && !HEADER_SET_ALLOWED.contains(&rel.as_str()) {
            offenders.push(format!("{rel}: attaches an Authorization header"));
        }
    }
    assert!(
        offenders.is_empty(),
        "the fleet token must leave this machine only through darkmux_fleet::peer:\n  {}",
        offenders.join("\n  ")
    );
}

/// The tripwire's own patterns, proven on samples, and the module cut.
#[test]
fn the_tripwire_sees_each_pattern() {
    for (src, reads, sets) in [
        ("let t = darkmux_flow::serve_token();", true, false),
        ("std::env::var(\"DARKMUX_SERVE_TOKEN\")", true, false),
        ("read_secret(\"darkmux-serve-token\")", true, false),
        ("req.set(\"AUTHORIZATION\", x)", false, true),
        ("req.header(\"authorization\", x)", false, true),
        ("client.get(u).bearer_auth(t)", false, true),
        ("println!(\"set DARKMUX_SERVE_TOKEN\")", false, false),
    ] {
        assert_eq!(findings(src), (reads, sets), "{src}");
    }
    let src = "#[cfg(test)]\nfn helper() {}\nfn prod() { serve_token(); }\n#[cfg(test)]\nmod tests { fn t() { serve_token(); } }";
    let prod = production_part(src);
    assert!(prod.contains("fn prod()") && !prod.contains("mod tests"), "{prod}");
}
