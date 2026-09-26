//! (#2916 re-review MUST 3) The fleet token leaves this machine through ONE
//! helper, `darkmux_fleet::peer` (`fleet_get` / `fleet_post_json`), which
//! only sends to a loopback target or a roster entry verified against the
//! identity provider. This test reads the workspace's non-test source and
//! fails if anything else reads the token (`serve_token()`) or attaches an
//! `Authorization` header with ureq (`.set("Authorization"`).
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
            if p.file_name().is_some_and(|n| n == "tests" || n == "target") {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The file's non-test part: everything before its first `#[cfg(test)]`
/// item. Files that are test modules as a whole are skipped by name.
fn production_part(src: &str) -> &str {
    match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    }
}

#[test]
fn the_fleet_token_is_attached_only_by_the_peer_helper() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    let crates = std::fs::read_dir(root.join("crates")).unwrap();
    for c in crates.flatten() {
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
        let prod = production_part(&src);
        if prod.contains("serve_token()") && !TOKEN_READ_ALLOWED.contains(&rel.as_str()) {
            offenders.push(format!("{rel}: reads the fleet token (serve_token())"));
        }
        if prod.contains(".set(\"Authorization\"") && !HEADER_SET_ALLOWED.contains(&rel.as_str()) {
            offenders.push(format!("{rel}: sets an Authorization header"));
        }
    }
    assert!(
        offenders.is_empty(),
        "the fleet token must leave this machine only through darkmux_fleet::peer:\n  {}",
        offenders.join("\n  ")
    );
}
