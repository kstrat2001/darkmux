//! The ratchet that keeps OS differences in this crate.
//!
//! Outside `darkmux-platform`, a line naming the OS (`target_os`,
//! `consts::OS`) is allowed only where one already was when platform
//! strategies arrived: `os-gates.allow` lists those per file. A file that
//! gains one fails here; the fix is a `Platform` method. A file that loses
//! one fails too, until its count in `os-gates.allow` is lowered, so the list
//! records the migration and only shrinks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const PATTERNS: [&str; 2] = ["target_os", "consts::OS"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Lines naming the OS, per workspace-relative file, outside this crate.
fn os_gates(root: &Path) -> BTreeMap<String, usize> {
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("tests"), &mut files);
    let crates = std::fs::read_dir(root.join("crates")).expect("crates/ exists");
    for krate in crates.flatten() {
        if krate.file_name() == "darkmux-platform" {
            continue;
        }
        rust_files(&krate.path().join("src"), &mut files);
        rust_files(&krate.path().join("tests"), &mut files);
    }
    let mut gates = BTreeMap::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let n = text
            .lines()
            .filter(|l| PATTERNS.iter().any(|p| l.contains(p)))
            .count();
        if n > 0 {
            let rel = file
                .strip_prefix(root)
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/");
            gates.insert(rel, n);
        }
    }
    gates
}

fn allowed() -> BTreeMap<String, usize> {
    include_str!("../os-gates.allow")
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (path, n) = l.rsplit_once(' ').expect("`<path> <count>`");
            (path.to_string(), n.parse().expect("count is a number"))
        })
        .collect()
}

/// Every difference between the found gates and the allowed ones, worded as
/// the fix.
fn ratchet_violations(
    found: &BTreeMap<String, usize>,
    allowed: &BTreeMap<String, usize>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (path, &n) in found {
        let max = allowed.get(path).copied().unwrap_or(0);
        if n > max {
            out.push(format!(
                "{path}: {n} lines name the OS, {max} allowed. Put the difference behind a \
                 `darkmux_platform::Platform` method instead of branching on the OS here."
            ));
        }
    }
    for (path, &max) in allowed {
        let n = found.get(path).copied().unwrap_or(0);
        if n < max {
            out.push(format!(
                "{path}: {n} lines name the OS now, {max} listed. Lower its line in \
                 crates/darkmux-platform/os-gates.allow (remove it at 0)."
            ));
        }
    }
    out
}

#[test]
fn os_differences_stay_in_the_platform_crate() {
    let violations = ratchet_violations(&os_gates(&workspace_root()), &allowed());
    assert!(violations.is_empty(), "\n{}", violations.join("\n"));
}

#[test]
fn the_ratchet_refuses_a_new_gate_and_a_stale_count() {
    let allowed = BTreeMap::from([("src/a.rs".to_string(), 2)]);
    let grew = BTreeMap::from([("src/a.rs".to_string(), 3)]);
    let new_file = BTreeMap::from([("src/a.rs".to_string(), 2), ("src/b.rs".to_string(), 1)]);
    let shrank = BTreeMap::from([("src/a.rs".to_string(), 1)]);
    assert!(ratchet_violations(&allowed, &allowed).is_empty());
    assert_eq!(ratchet_violations(&grew, &allowed).len(), 1);
    assert!(ratchet_violations(&new_file, &allowed)[0].starts_with("src/b.rs"));
    assert!(ratchet_violations(&shrank, &allowed)[0].contains("Lower its line"));
}
