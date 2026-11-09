//! (#3457) Conformance: every production `git` spawn goes through
//! `darkmux_types::host_git::HostGit`.
//!
//! A dispatch container mounts its workdir read-write, so a run can write the
//! `.git` that host git reads there (or replace a worktree's `.git` pointer
//! file with a directory of its own). A bare `Command::new("git")` discovers
//! that `.git` and applies its config, and several settings run a command
//! (`core.fsmonitor`, filters, textconv, hooks). `HostGit` is the one place
//! the hardening is written: the `-c` overrides, `GIT_CONFIG_NOSYSTEM`, and
//! the explicit `--git-dir` for a work tree a run wrote.
//!
//! **A lint, not an enumeration**, in the shape of
//! `crates/darkmux-profiles/tests/pin_cwd_conformance.rs`: it walks every
//! `.rs` file under the root crate's `src/` and each workspace crate's
//! `src/`, skips test code (`*_tests.rs`, `tests.rs`, and any item under a
//! `#[cfg(test)]` attribute), and fails on any line in a file not on
//! [`ALLOWED`] that spawns git in one of the shapes [`is_git_spawn`] knows:
//! `Command::new("git")` or `Command::new("/usr/bin/git")`, `Command::new(X)`
//! where the file declares `const X: &str = "git"`, the program built as a
//! value (`"git".to_string()`, `String::from("git")`, `OsStr::new("git")`),
//! and a shell string running git (`"git status"`, `"…; git …"`) near a
//! `"-c"`. A spawn written some other way (the program read from config, or
//! assembled from pieces) is not seen.
//! `runtime/` is not scanned: it runs inside the container, where git reads
//! only what the container can see.
//!
//! Each [`ALLOWED`] entry names its reason, and must still contain a spawn:
//! a stale entry fails too, so the list only shrinks.

use std::path::{Path, PathBuf};

/// Ways of naming git as a program VALUE, outside `Command::new`.
const VALUE_SHAPES: [&str; 6] = [
    "\"git\".to_string()",
    "\"git\".to_owned()",
    "\"git\".into()",
    "String::from(\"git\")",
    "OsStr::new(\"git\")",
    "OsString::from(\"git\")",
];

/// How a shell string starts git: at its start or after a separator.
const SHELL_GIT: [&str; 7] = ["\"git ", "; git ", "&& git ", "|| git ", "| git ", "$(git ", "\\ngit "];

/// The string literals on a code line, roughly: the text between pairs of
/// unescaped quotes.
fn literals(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    let mut chars = code.chars();
    while let Some(c) = chars.next() {
        match (&mut cur, c) {
            (Some(buf), '\\') => {
                buf.push('\\');
                if let Some(n) = chars.next() {
                    buf.push(n);
                }
            }
            (Some(_), '"') => out.push(cur.take().unwrap()),
            (Some(buf), c) => buf.push(c),
            (None, '"') => cur = Some(String::new()),
            (None, _) => {}
        }
    }
    out
}

/// The names a file binds to the git program: `const X: &str = "git";`.
fn git_consts(src: &str) -> Vec<String> {
    src.lines()
        .filter_map(|l| {
            let t = l.trim_start().trim_start_matches("pub(crate) ").trim_start_matches("pub ");
            let rest = t.strip_prefix("const ").or_else(|| t.strip_prefix("static "))?;
            let (name, value) = rest.split_once(':')?;
            let lit = value.split_once('=')?.1.trim().trim_end_matches(';');
            let lit = lit.strip_prefix('"')?.strip_suffix('"')?;
            (lit == "git" || lit.ends_with("/git")).then(|| name.trim().to_string())
        })
        .collect()
}

/// Whether `code` (a line with its comment cut) spawns git in a known shape.
/// `near_dash_c`: a `"-c"` is on this line or one of the three before it in
/// the same statement.
fn is_git_spawn(code: &str, consts: &[String], near_dash_c: bool) -> bool {
    let mut rest = code;
    while let Some(at) = rest.find("Command::new(") {
        rest = &rest[at + "Command::new(".len()..];
        let arg = rest.trim_start();
        if let Some(lit) = arg.strip_prefix('"').and_then(|a| a.split('"').next()) {
            if lit == "git" || lit.ends_with("/git") {
                return true;
            }
        }
        let ident: String = arg.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        if consts.contains(&ident) {
            return true;
        }
    }
    if VALUE_SHAPES.iter().any(|v| code.contains(v)) {
        return true;
    }
    near_dash_c
        && literals(code).iter().any(|l| {
            let quoted = format!("\"{l}");
            SHELL_GIT.iter().any(|g| quoted.starts_with(g) || l.contains(g.trim_start_matches('"')) && !g.starts_with('"'))
        })
}

/// Production files that may spawn git directly: the path, how many spawns
/// it may hold (`None`: any number; `Some(n)`: exactly `n`, so a new spawn
/// beside an allowed one still fails), and the reason.
const ALLOWED: &[(&str, Option<usize>, &str)] = &[
    ("crates/darkmux-types/src/host_git.rs", None, "the helper itself"),
    (
        "crates/darkmux-types/src/host_git_fixture.rs",
        None,
        "test-support only (`test-support` feature): plays the run's side, building and planting repos",
    ),
    (
        "crates/darkmux-crew/src/workspace_spec/materialize.rs",
        None,
        "bare mirrors and their detached snapshot worktrees, outside every dispatch's read-write mount; \
         clone and fetch need the operator's own credential helper and ssh command, which HostGit's \
         flags override",
    ),
    (
        "crates/darkmux-serve/src/client_workspace.rs",
        Some(1),
        "`git_bounded`: a paired app's clone and fetch, which need the operator's own credential helper \
         and ssh command (HostGit clears both). It takes those from the GLOBAL config only, turns hooks \
         and fsmonitor off, and a fetch first refuses a checkout whose own config sets a network \
         setting no override can switch off",
    ),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").canonicalize().unwrap()
}

/// Every `src/` tree the lint reads: the root crate's and each member's.
fn source_roots(root: &Path) -> Vec<PathBuf> {
    let mut roots = vec![root.join("src")];
    let mut crates: Vec<PathBuf> = std::fs::read_dir(root.join("crates"))
        .unwrap()
        .flatten()
        .map(|e| e.path().join("src"))
        .filter(|p| p.is_dir())
        .collect();
    crates.sort();
    roots.extend(crates);
    roots
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "tests") {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            let name = p.file_name().unwrap().to_string_lossy();
            if name == "tests.rs" || name.ends_with("_tests.rs") {
                continue;
            }
            out.push(p);
        }
    }
}

/// Scanner state carried across lines: whether we are inside a string or a
/// block comment, so braces in them are not counted.
#[derive(Default)]
struct Lexer {
    in_str: bool,
    raw_hashes: Option<usize>,
    in_block_comment: bool,
}

impl Lexer {
    /// The net brace depth change of `line`, counting only code.
    fn brace_delta(&mut self, line: &str) -> i64 {
        let b = line.as_bytes();
        let mut i = 0;
        let mut delta = 0;
        while i < b.len() {
            if self.in_block_comment {
                if b[i..].starts_with(b"*/") {
                    self.in_block_comment = false;
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            if let Some(h) = self.raw_hashes {
                if b[i] == b'"' && b[i + 1..].iter().take(h).filter(|c| **c == b'#').count() == h {
                    self.raw_hashes = None;
                    i += 1 + h;
                } else {
                    i += 1;
                }
                continue;
            }
            if self.in_str {
                match b[i] {
                    b'\\' => i += 2,
                    b'"' => {
                        self.in_str = false;
                        i += 1;
                    }
                    _ => i += 1,
                }
                continue;
            }
            match b[i] {
                b'/' if b.get(i + 1) == Some(&b'/') => break,
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    self.in_block_comment = true;
                    i += 2;
                }
                b'r' if matches!(b.get(i + 1), Some(b'"' | b'#')) && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) => {
                    let hashes = b[i + 1..].iter().take_while(|c| **c == b'#').count();
                    if b.get(i + 1 + hashes) == Some(&b'"') {
                        self.raw_hashes = Some(hashes);
                        i += 2 + hashes;
                    } else {
                        i += 1;
                    }
                }
                b'"' => {
                    self.in_str = true;
                    i += 1;
                }
                // A char literal: '{', '\'', '\u{7b}'. A lifetime has no closing quote.
                b'\'' => {
                    if b.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        while i < b.len() && b[i] != b'\'' {
                            i += 1;
                        }
                        i += 1;
                    } else if b.get(i + 2) == Some(&b'\'') {
                        i += 3;
                    } else {
                        i += 1;
                    }
                }
                b'{' => {
                    delta += 1;
                    i += 1;
                }
                b'}' => {
                    delta -= 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        delta
    }
}

/// An attribute line that makes the next item test-only.
fn is_test_attr(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("#[cfg(test)]") || (t.starts_with("#[cfg(any(test") && !t.contains("not(test"))
}

/// The 1-based line numbers of production `git` spawns in `src`.
fn production_spawns(src: &str) -> Vec<usize> {
    let consts = git_consts(src);
    let mut recent: std::collections::VecDeque<bool> = std::collections::VecDeque::new();
    let mut hits = Vec::new();
    let mut lexer = Lexer::default();
    // Some(depth) while skipping a test item that opened at `depth`.
    let mut depth: i64 = 0;
    let mut skipping: Option<i64> = None;
    let mut pending_test_attr = false;
    for (n, line) in src.lines().enumerate() {
        let code_starts_in_string = lexer.in_str || lexer.raw_hashes.is_some() || lexer.in_block_comment;
        let before = depth;
        depth += lexer.brace_delta(line);
        if let Some(open_at) = skipping {
            if depth <= open_at && line.contains('}') {
                skipping = None;
            }
            continue;
        }
        if pending_test_attr {
            let t = line.trim_start();
            if t.starts_with("#[") || t.is_empty() || t.starts_with("//") {
                continue;
            }
            pending_test_attr = false;
            if depth > before {
                skipping = Some(before);
            }
            // A one-line item (`use …;`, `mod x;`) or one that opened and
            // closed on this line is skipped by not scanning it.
            continue;
        }
        if !code_starts_in_string && is_test_attr(line) {
            pending_test_attr = true;
            continue;
        }
        let code = line.split("//").next().unwrap_or("");
        let dash_c = code.contains("\"-c\"");
        if is_git_spawn(code, &consts, dash_c || recent.iter().any(|d| *d)) {
            hits.push(n + 1);
        }
        recent.push_back(dash_c);
        if recent.len() > 3 {
            recent.pop_front();
        }
        // The window is one statement: it ends where the statement does.
        if code.trim_end().ends_with(';') || code.trim_end().ends_with('}') {
            recent.clear();
        }
    }
    hits
}

#[test]
fn every_production_git_spawn_goes_through_host_git() {
    let root = workspace_root();
    let mut files = Vec::new();
    for dir in source_roots(&root) {
        rust_files(&dir, &mut files);
    }
    assert!(files.len() > 100, "the walk found too few files ({}): wrong root?", files.len());
    let mut offenders = Vec::new();
    let mut allowed_seen = Vec::new();
    for file in &files {
        let rel = file.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        let hits = production_spawns(&std::fs::read_to_string(file).unwrap());
        if hits.is_empty() {
            continue;
        }
        if let Some((_, count, _)) = ALLOWED.iter().find(|(p, _, _)| *p == rel) {
            if count.is_some_and(|n| n != hits.len()) {
                offenders.push(format!("{rel}: {} spawns, ALLOWED says {count:?}", hits.len()));
            }
            allowed_seen.push(rel);
            continue;
        }
        for line in hits {
            offenders.push(format!("{rel}:{line}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "production code spawns git directly; build it with `darkmux_types::host_git::HostGit` \
         (trusted for an operator-owned checkout, run_written with a held --git-dir for a tree a \
         run wrote), or add the file to ALLOWED with its reason:\n  {}",
        offenders.join("\n  ")
    );
    let stale: Vec<&str> =
        ALLOWED.iter().map(|(p, _, _)| *p).filter(|p| !allowed_seen.iter().any(|s| s == p)).collect();
    assert!(stale.is_empty(), "ALLOWED entries with no git spawn left (remove them): {stale:?}");
}

#[test]
fn the_scanner_skips_test_items_and_sees_production_spawns() {
    let src = r#"
fn prod() { let _ = std::process::Command::new("git"); }
// a comment naming Command::new("git") is not a spawn
#[cfg(test)]
mod tests {
    fn t() { let s = "{"; let c = '{'; let _ = Command::new("git"); }
}
fn after() {
    let _ = Command::new("git");
}
#[cfg(test)]
fn helper() -> String {
    format!("{}", Command::new("git").get_program().len())
}
fn last() { Command::new("git"); }
const GIT: &str = "git";
fn by_const() { Command::new(GIT).status(); }
fn by_path() { tokio::process::Command::new("/usr/bin/git"); }
fn by_value() { let p = "git".to_string(); spawn(p); }
fn shell() {
    Command::new("sh")
        .arg("-c")
        .arg("cd /x && git status");
}
fn shell_start() { Command::new("bash").args(["-c", "git diff"]); }
fn not_git() { Command::new("gitk"); let m = "git worktree add failed"; eprintln!("{m}"); }
fn json_key() { let v = obj.get("git"); }
"#;
    assert_eq!(production_spawns(src), vec![2, 9, 15, 17, 18, 19, 23, 25]);
}
