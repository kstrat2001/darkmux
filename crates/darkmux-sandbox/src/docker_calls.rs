//! (#3112) The ratchet that moves Docker behind the sandbox seam.
//!
//! In production code outside the Docker backend (`src/docker.rs` here), a
//! line spelling the `docker` program as a string literal (a
//! `Command::new`, an argv's first word, a `PATH` probe) is allowed only
//! where one already was when the seam arrived: `docker-calls.allow` names
//! each file and its count. A file that gains one fails here; the fix is to
//! go through `SandboxBackend`. A file that loses one fails too, until its
//! count is lowered, so the list records the migration and only shrinks.
//!
//! Test code is not counted: `tests/` directories, `tests.rs` and
//! `*_tests.rs` files, and every `#[cfg(test)]` item. Nor is a
//! `config_enum!` row, which names a setting's value (`runtime.sandbox`'s
//! `docker`, #3112), not the program. It reads text, so a
//! comment spelling the literal counts too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const LITERAL: &str = "\"docker\"";

/// The backend itself, the one file that may spell the program.
const BACKEND: &str = "crates/darkmux-sandbox/src/docker.rs";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn is_test_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    name == "tests.rs" || name.ends_with("_tests.rs")
}

fn production_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "tests") {
                production_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") && !is_test_file(&path) {
            out.push(path);
        }
    }
}

/// `text`'s lines outside `#[cfg(test)]` items. An item is the next line
/// ending in `;`, `}` or `{` after the attribute (other attributes and a
/// multi-line signature between), or the item's own line when it ends in
/// `,` (a match arm, a field); a block item runs to the line at its own
/// indent that is `}` followed only by `)`, `;` or `,` (`}`, `};`, `});`).
fn production_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim() != "#[cfg(test)]" {
            out.push(line);
            continue;
        }
        skip_test_item(&mut lines);
    }
    out
}

fn skip_test_item<'a>(lines: &mut impl Iterator<Item = &'a str>) {
    let mut indent = None;
    for line in lines.by_ref() {
        let trimmed = line.trim_end();
        if indent.is_none() && !trimmed.trim_start().starts_with("#[") {
            indent = Some(&line[..line.len() - line.trim_start().len()]);
            if trimmed.ends_with(',') {
                return;
            }
        }
        // A declaration, or a one-line item (`fn f() {}`) closed on its own
        // line.
        if trimmed.ends_with(';') || trimmed.ends_with('}') {
            return;
        }
        if trimmed.ends_with('{') {
            let indent = indent.unwrap_or_default();
            lines.by_ref().find(|l| closes_at(l, indent));
            return;
        }
    }
}

/// Whether `line` closes a block opened at `indent`: that indent, then `}`
/// and nothing but `)`, `;` or `,` (not a raw string's `}"#;`).
fn closes_at(line: &str, indent: &str) -> bool {
    line.strip_prefix(indent)
        .and_then(|rest| rest.trim_end().strip_prefix('}'))
        .is_some_and(|tail| tail.chars().all(|c| matches!(c, ')' | ';' | ',')))
}

/// Every Rust package the repo builds: the root, each one under `crates/`,
/// `plugins/` and `tools/`, and `runtime/` (outside the workspace).
fn packages(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![root.to_path_buf(), root.join("runtime")];
    for group in ["crates", "plugins", "tools"] {
        let Ok(entries) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        out.extend(entries.flatten().map(|e| e.path()));
    }
    out
}

/// Production lines spelling the program, per workspace-relative file,
/// outside the backend.
fn docker_calls(root: &Path) -> BTreeMap<String, usize> {
    let mut files = Vec::new();
    for package in packages(root) {
        production_files(&package.join("src"), &mut files);
        let build = package.join("build.rs");
        if build.is_file() {
            files.push(build);
        }
    }
    let mut calls = BTreeMap::new();
    for file in files {
        let rel = file.strip_prefix(root).unwrap_or(&file).to_string_lossy().replace('\\', "/");
        if rel == BACKEND {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let n = production_lines(&text).iter().filter(|l| spells_the_program(l)).count();
        if n > 0 {
            calls.insert(rel, n);
        }
    }
    calls
}

/// Whether `line` spells the program: it holds the literal, and is not a
/// `config_enum!` row (`Variant = <the literal> => "<meaning>",`), which
/// declares a setting's value (#3112: `runtime.sandbox`), with the literal
/// once.
fn spells_the_program(line: &str) -> bool {
    if !line.contains(LITERAL) {
        return false;
    }
    let token_row = line.trim_start().split_once(&format!(" = {LITERAL} => \"")).is_some_and(|(variant, meaning)| {
        variant.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !meaning.contains(LITERAL)
    });
    !token_row
}

fn allowed() -> BTreeMap<String, usize> {
    include_str!("../docker-calls.allow")
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (path, n) = l.rsplit_once(' ').expect("`<path> <count>`");
            (path.to_string(), n.parse().expect("count is a number"))
        })
        .collect()
}

/// Every difference between the found calls and the allowed ones, worded as
/// the fix.
fn ratchet_violations(found: &BTreeMap<String, usize>, allowed: &BTreeMap<String, usize>) -> Vec<String> {
    let mut out = Vec::new();
    for (path, &n) in found {
        let max = allowed.get(path).copied().unwrap_or(0);
        if n > max {
            out.push(format!(
                "{path}: {n} lines spell the `docker` program, {max} allowed. Build a \
                 `darkmux_sandbox::SandboxPolicy` and spawn what a `SandboxBackend` returns instead."
            ));
        }
    }
    for (path, &max) in allowed {
        let n = found.get(path).copied().unwrap_or(0);
        if n < max {
            out.push(format!(
                "{path}: {n} lines spell the `docker` program now, {max} listed. Lower its line in \
                 crates/darkmux-sandbox/docker-calls.allow (remove it at 0)."
            ));
        }
    }
    out
}

#[test]
fn docker_is_spelled_only_by_the_sandbox_backend() {
    let violations = ratchet_violations(&docker_calls(&workspace_root()), &allowed());
    assert!(violations.is_empty(), "\n{}", violations.join("\n"));
}

#[test]
fn the_ratchet_refuses_a_new_call_and_a_stale_count() {
    let allowed = BTreeMap::from([("src/a.rs".to_string(), 2)]);
    let grew = BTreeMap::from([("src/a.rs".to_string(), 3)]);
    let new_file = BTreeMap::from([("src/a.rs".to_string(), 2), ("src/b.rs".to_string(), 1)]);
    let shrank = BTreeMap::from([("src/a.rs".to_string(), 1)]);
    assert!(ratchet_violations(&allowed, &allowed).is_empty());
    assert_eq!(ratchet_violations(&grew, &allowed).len(), 1);
    assert!(ratchet_violations(&new_file, &allowed)[0].starts_with("src/b.rs"));
    assert!(ratchet_violations(&shrank, &allowed)[0].contains("Lower its line"));
}

/// Test items are not production: a `#[cfg(test)]` module, function,
/// one-line item or `mod x;` declaration is skipped through its end, and
/// the lines after it count again.
#[test]
fn test_items_are_skipped_and_production_around_them_counts() {
    let text = [
        "a \"docker\"",
        "#[cfg(test)]",
        "mod tests;",
        "#[cfg(test)]",
        "#[path = \"x.rs\"]",
        "fn helper(",
        "    x: u8,",
        ") -> u8 {",
        "    \"docker\";",
        "    {",
        "    }",
        "}",
        "b \"docker\"",
        "    #[cfg(test)]",
        "    mod tests {",
        "        \"docker\"",
        "    }",
        "c \"docker\"",
        "#[cfg(test)]",
        "fn one_line() {}",
        "d \"docker\"",
        "#[cfg(test)]",
        "fn one_line_with_body() -> u8 { 1 }",
        "e \"docker\"",
        // A struct-literal item closes with `};`, not a bare `}` (#3552
        // review): the item ends there, not at the end of the file.
        "#[cfg(test)]",
        "const R: std::ops::Range<u8> = std::ops::Range {",
        "    start: 0,",
        "    end: \"docker\".len() as u8,",
        "};",
        "f \"docker\"",
        "    #[cfg(test)]",
        "    static S: Wrap = Wrap(Inner {",
        "        x: \"docker\",",
        "    });",
        "g \"docker\"",
        // A raw string's line at the module's indent that starts with `}`
        // is not the module's close (#3552 re-review).
        "#[cfg(test)]",
        "mod fixtures {",
        "    const J: &str = r#\"{",
        "}\"#;",
        "    \"docker\"",
        "}",
        "h \"docker\"",
        // A `#[cfg(test)]` match arm or field is its own line: the sibling
        // after it is production (#3552 re-review).
        "    #[cfg(test)]",
        "    0 => Command::new(\"true\"),",
        "    _ => Command::new(\"docker\"),",
    ]
    .join("\n");
    let counted: Vec<&str> = production_lines(&text).into_iter().filter(|l| l.contains(LITERAL)).collect();
    assert_eq!(
        counted,
        vec![
            "a \"docker\"",
            "b \"docker\"",
            "c \"docker\"",
            "d \"docker\"",
            "e \"docker\"",
            "f \"docker\"",
            "g \"docker\"",
            "h \"docker\"",
            "    _ => Command::new(\"docker\"),",
        ]
    );
}

/// (#3112) A `config_enum!` row is a setting's value (`runtime.sandbox`'s
/// `docker`), not the program: it does not count. Anything else on such a
/// line, or the literal anywhere else, still does.
#[test]
fn a_settings_token_row_is_not_a_program_spelling() {
    assert!(!spells_the_program("    Docker = \"docker\" => \"a Docker container\","));
    assert!(spells_the_program("    Docker = \"docker\","));
    assert!(spells_the_program("    let docker = \"docker\";"));
    assert!(spells_the_program("    Command::new(\"docker\")"));
    assert!(spells_the_program("    X = \"docker\" => Command::new(\"docker\"),"));
}

/// The walk sees production files only, in every Rust tree the repo builds
/// (the workspace, `runtime/`, `plugins/`, `tools/`, and build scripts),
/// and finds a literal the way the ratchet counts it, so a pass is not an
/// empty scan.
#[test]
fn the_scan_counts_production_files_and_skips_test_files() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let write = |rel: &str, body: &str| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    };
    write("src/main.rs", "Command::new(\"docker\")\n");
    write("crates/x/src/lib.rs", "on_path(\"docker\")\n\"docker_on_path\"\n");
    write("crates/x/src/lib_tests.rs", "\"docker\"\n");
    write("crates/x/src/sub/tests.rs", "\"docker\"\n");
    write("crates/x/tests/proof.rs", "\"docker\"\n");
    write("crates/x/src/tests/helper.rs", "\"docker\"\n");
    write(BACKEND, "\"docker\"\n");
    write("crates/x/build.rs", "\"docker\"\n");
    write("runtime/src/main.rs", "\"docker\"\n");
    write("plugins/p/src/lib.rs", "\"docker\"\n");
    write("tools/t/src/main.rs", "\"docker\"\n");
    let found = docker_calls(&root);
    let one = |p: &str| (p.to_string(), 1);
    assert_eq!(
        found,
        BTreeMap::from([
            one("crates/x/build.rs"),
            one("crates/x/src/lib.rs"),
            one("plugins/p/src/lib.rs"),
            one("runtime/src/main.rs"),
            one("src/main.rs"),
            one("tools/t/src/main.rs"),
        ])
    );
}
