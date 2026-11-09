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
//! [`ALLOWED`] that names git in one of the shapes [`is_git_spawn`] knows:
//! any string literal (plain or raw) that is `git` or ends in `/git` and is
//! not a key or a comparison (so a binding, a wrapper's argument, `xcrun` or
//! `env` running it, a constant and a spawn split over lines are all seen
//! where the name is written), `Command::new(X)` where any scanned file
//! declares `const X: &str = "git"`, and a shell string running git (`"git
//! status"`, `"…; git …"`, `"exec git …"`, `"env git …"`) near a `"-c"` or
//! `"-lc"`. A spawn written some other way (the program read from config,
//! or assembled from pieces) is not seen.
//! `runtime/` is not scanned: it runs inside the container, where git reads
//! only what the container can see.
//!
//! Each [`ALLOWED`] entry names its reason, and must still contain a spawn:
//! a stale entry fails too, so the list only shrinks.

use std::path::{Path, PathBuf};

/// Shell words that run the command after them (`exec git`, `env -i git`).
const SHELL_PREFIXES: [&str; 6] = ["exec", "env", "xcrun", "command", "nohup", "time"];

/// A string literal on a code line: its byte span (from its opening quote,
/// or the `r` of a raw string, to just past its closing quote) and its text.
struct Lit {
    start: usize,
    end: usize,
    text: String,
}

/// The string literals on a code line (plain, with escapes kept as written,
/// and raw), and where its code ends (a `//` comment outside a literal).
/// Char literals and lifetimes are stepped over.
fn literals(code: &str) -> (Vec<Lit>, usize) {
    let c: Vec<(usize, char)> = code.char_indices().collect();
    let at = |i: usize| c.get(i).map(|x| x.1);
    let byte = |i: usize| c.get(i).map_or(code.len(), |x| x.0);
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i].1;
        let after_ident = i > 0 && (c[i - 1].1.is_alphanumeric() || c[i - 1].1 == '_');
        if ch == '/' && at(i + 1) == Some('/') {
            return (out, byte(i));
        }
        if ch == '\'' {
            if at(i + 1) == Some('\\') {
                i += 2;
                while i < c.len() && c[i].1 != '\'' {
                    i += 1;
                }
                i += 1;
            } else if at(i + 2) == Some('\'') {
                i += 3;
            } else {
                i += 1;
            }
            continue;
        }
        let hashes = c[(i + 1).min(c.len())..].iter().take_while(|x| x.1 == '#').count();
        if ch == 'r' && !after_ident && at(i + 1 + hashes) == Some('"') {
            let body = i + 2 + hashes;
            let mut j = body;
            while j < c.len() && !(c[j].1 == '"' && c[j + 1..].iter().take(hashes).filter(|x| x.1 == '#').count() == hashes) {
                j += 1;
            }
            let end = (j + 1 + hashes).min(c.len());
            out.push(Lit { start: byte(i), end: byte(end), text: c[body..j.min(c.len())].iter().map(|x| x.1).collect() });
            i = end;
            continue;
        }
        if ch == '"' {
            let mut j = i + 1;
            let mut text = String::new();
            while j < c.len() && c[j].1 != '"' {
                if c[j].1 == '\\' && j + 1 < c.len() {
                    text.push('\\');
                    j += 1;
                }
                text.push(c[j].1);
                j += 1;
            }
            out.push(Lit { start: byte(i), end: byte(j + 1), text });
            i = j + 1;
            continue;
        }
        i += 1;
    }
    (out, code.len())
}

/// A literal that names the git program.
fn names_git(text: &str) -> bool {
    text == "git" || text.ends_with("/git")
}

/// Whether `lit` on `code` is a key or a comparison operand rather than a
/// program: `{"git": …}`, `x == "git"`, `obj.get("git")`, `s.contains("git")`.
fn is_key_or_comparison(code: &str, lit: &Lit) -> bool {
    let after = code[lit.end..].trim_start();
    let before = code[..lit.start].trim_end();
    (after.starts_with(':') && !after.starts_with("::"))
        || after.starts_with("=>")
        || ["==", "!=", ".get(", ".remove(", "contains(", "contains_key(", "starts_with(", "ends_with(", ".eq("]
            .iter()
            .any(|p| before.ends_with(p))
}

/// Whether a shell string runs git: `git` (or a path ending `/git`) as a
/// command word, at the start, after a separator, or after a word that runs
/// what follows it ([`SHELL_PREFIXES`], with any flags or `VAR=value`).
fn shell_runs_git(text: &str) -> bool {
    let text = text.replace("\\n", " ; ");
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() || ";&|()`{}$".contains(ch) {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            if !ch.is_whitespace() {
                words.push(ch.to_string());
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let mut at_command = true;
    let mut after_prefix = false;
    for w in &words {
        if w.len() == 1 && ";&|()`{}$".contains(w.as_str()) {
            (at_command, after_prefix) = (true, false);
            continue;
        }
        if at_command && names_git(w) {
            return true;
        }
        let prefix = SHELL_PREFIXES.contains(&w.as_str()) || (w.contains('=') && !w.starts_with('-'));
        after_prefix = prefix || (after_prefix && w.starts_with('-'));
        at_command = at_command && after_prefix;
    }
    false
}

/// A `-c` style flag (`"-c"`, `"-lc"`, `"-ec"`): what hands a shell (or
/// git) the next argument as its script.
fn is_dash_c(lit: &Lit) -> bool {
    let t = &lit.text;
    t.len() >= 2 && t.len() <= 4 && t.starts_with('-') && t.ends_with('c') && t[1..].chars().all(|c| c.is_ascii_lowercase())
}

/// The names a file binds to the git program: `const X: &str = "git";`
/// (or a raw string).
fn git_consts(src: &str) -> Vec<String> {
    src.lines()
        .filter_map(|l| {
            let t = l.trim_start().trim_start_matches("pub(crate) ").trim_start_matches("pub ");
            let rest = t.strip_prefix("const ").or_else(|| t.strip_prefix("static "))?;
            let (name, value) = rest.split_once(':')?;
            let (lits, _) = literals(value.split_once('=')?.1);
            (lits.len() == 1 && names_git(&lits[0].text)).then(|| name.trim().to_string())
        })
        .collect()
}

/// Whether `code` (one line, its comment cut) spawns git in a known shape.
/// `consts`: the git constants of every scanned file. `near_dash_c`: a
/// `"-c"` is on this line or one of the three before it in the same
/// statement.
fn is_git_spawn(code: &str, lits: &[Lit], consts: &[String], near_dash_c: bool) -> bool {
    if lits.iter().any(|l| names_git(&l.text) && !is_key_or_comparison(code, l)) {
        return true;
    }
    let mut rest = code;
    while let Some(at) = rest.find("Command::new(") {
        rest = &rest[at + "Command::new(".len()..];
        let path: String = rest.trim_start().chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':').collect();
        if consts.iter().any(|k| path.rsplit("::").next() == Some(k.as_str())) {
            return true;
        }
    }
    near_dash_c && lits.iter().any(|l| shell_runs_git(&l.text))
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
        "`git_bounded_with`: a paired app's clone and fetch, which need the operator's own credential helper \
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

/// `line` as code to scan, given the lexer state it starts in: a line that
/// opens inside a string gets that string's opening quote back, so its text
/// still reads as a literal; one that opens inside a block comment starts
/// after the comment (`None`: the whole line is comment).
fn scannable(line: &str, lexer: &Lexer) -> Option<String> {
    if lexer.in_block_comment {
        return line.split_once("*/").map(|(_, rest)| rest.to_string());
    }
    Some(match lexer.raw_hashes {
        Some(h) => format!("r{}\"{line}", "#".repeat(h)),
        None if lexer.in_str => format!("\"{line}"),
        None => line.to_string(),
    })
}

/// The 1-based line numbers of production `git` spawns in `src`. `consts`:
/// the git constants of every scanned file.
fn production_spawns(src: &str, consts: &[String]) -> Vec<usize> {
    let mut recent: std::collections::VecDeque<bool> = std::collections::VecDeque::new();
    let mut hits = Vec::new();
    let mut lexer = Lexer::default();
    // Some(depth) while skipping a test item that opened at `depth`.
    let mut depth: i64 = 0;
    let mut skipping: Option<i64> = None;
    let mut pending_test_attr = false;
    let mut opens_new = false;
    for (n, line) in src.lines().enumerate() {
        let code_starts_in_string = lexer.in_str || lexer.raw_hashes.is_some() || lexer.in_block_comment;
        let scanned = scannable(line, &lexer);
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
        let Some(scanned) = scanned else { continue };
        let (lits, cut) = literals(&scanned);
        let code = &scanned[..cut];
        // `Command::new(` closing on a later line: its argument opens this one.
        let code = match opens_new {
            true => format!("Command::new({}", code.trim_start()),
            false => code.to_string(),
        };
        let shift = code.len() - scanned[..cut].len();
        let lits: Vec<Lit> =
            lits.into_iter().map(|l| Lit { start: l.start + shift, end: l.end + shift, text: l.text }).collect();
        let dash_c = lits.iter().any(is_dash_c);
        if is_git_spawn(&code, &lits, consts, dash_c || recent.iter().any(|d| *d)) {
            hits.push(n + 1);
        }
        opens_new = code.trim_end().ends_with("Command::new(");
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

/// The production spawns of each `(path, source)` with any, a git constant
/// declared in one file counting in every other.
fn spawns_by_file(sources: &[(String, String)]) -> Vec<(String, Vec<usize>)> {
    let consts: Vec<String> = sources.iter().flat_map(|(_, s)| git_consts(s)).collect();
    sources
        .iter()
        .map(|(rel, src)| (rel.clone(), production_spawns(src, &consts)))
        .filter(|(_, hits)| !hits.is_empty())
        .collect()
}

#[test]
fn every_production_git_spawn_goes_through_host_git() {
    let root = workspace_root();
    let mut files = Vec::new();
    for dir in source_roots(&root) {
        rust_files(&dir, &mut files);
    }
    assert!(files.len() > 100, "the walk found too few files ({}): wrong root?", files.len());
    let sources: Vec<(String, String)> = files
        .iter()
        .map(|f| (f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"), std::fs::read_to_string(f).unwrap()))
        .collect();
    let mut offenders = Vec::new();
    let mut allowed_seen = Vec::new();
    for (rel, hits) in spawns_by_file(&sources) {
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
    let consts = git_consts(src);
    assert_eq!(production_spawns(src, &consts), vec![2, 9, 15, 16, 17, 18, 19, 23, 25]);
}

/// A constant naming git in one file makes `Command::new` of it a spawn in
/// another, wherever the path to it is spelled.
#[test]
fn a_git_constant_counts_in_every_file() {
    let sources = [
        ("a.rs".to_string(), "pub const GIT_BIN: &str = \"git\";\n".to_string()),
        ("b.rs".to_string(), "fn f() {\n    Command::new(crate::a::GIT_BIN).status();\n}\n".to_string()),
    ];
    let found = spawns_by_file(&sources);
    assert!(found.contains(&("b.rs".to_string(), vec![2])), "{found:?}");
}

/// Each shape a spawn can be written in, every one seen; and the shapes
/// that only mention git, none seen. One case per shape, so a scanner change
/// that loses one names it.
#[test]
fn each_spawn_shape_is_seen_and_each_mention_is_not() {
    let other_file = "pub const GIT_BIN: &str = \"git\";";
    let consts = git_consts(other_file);
    assert_eq!(consts, ["GIT_BIN"], "a constant in another file is collected");
    let spawns = [
        ("a let binding", "fn f() { let prog = \"git\"; Command::new(prog).status(); }"),
        ("a line break", "fn f() {\n    Command::new(\n        \"git\",\n    )\n    .status();\n}"),
        ("a raw string", "fn f() { Command::new(r\"git\").status(); }"),
        ("a raw string with hashes", "fn f() { Command::new(r#\"/usr/bin/git\"#).status(); }"),
        ("xcrun git", "fn f() { Command::new(\"xcrun\").arg(\"git\").status(); }"),
        ("env git", "fn f() { Command::new(\"/usr/bin/env\").args([\"git\", \"status\"]).status(); }"),
        ("a wrapper fn", "fn tool(p: &str) -> Command { Command::new(p) }\nfn f() { tool(\"git\").status(); }"),
        ("a const from another file", "fn f() { Command::new(crate::tools::GIT_BIN).status(); }"),
        ("a const from another file, on the next line", "fn f() {\n    Command::new(\n        GIT_BIN,\n    );\n}"),
        ("sh -c \"exec git\"", "fn f() { Command::new(\"sh\").args([\"-c\", \"exec git status\"]).status(); }"),
        ("sh -lc \"env git\"", "fn f() { Command::new(\"sh\").args([\"-lc\", \"cd x && env -i GIT_DIR=y git log\"]); }"),
        ("a multi-line shell script", "fn f() {\n    Command::new(\"sh\").arg(\"-c\").arg(\"set -e\n    git status\");\n}"),
        ("a spawn after a URL on the line", "fn f() { run(\"https://example.com\", \"git\"); }"),
        ("a spawn after a raw string ending in a backslash", "fn f() { let sep = r\"\\\"; Command::new(\"env\").arg(\"git\"); }"),
    ];
    for (shape, src) in spawns {
        assert!(!production_spawns(src, &consts).is_empty(), "{shape} is not seen:\n{src}");
    }
    let mentions = [
        ("a json key", "fn f() { let v = obj.get(\"git\"); let j = json!({\"git\": url}); }"),
        ("a comparison", "fn f() { if ext == \"git\" || s.ends_with(\"git\") {} }"),
        ("a match arm", "fn f() { match k { \"git\" => 1, _ => 2 }; }"),
        ("a message", "fn f() { let m = \"git worktree add failed\"; eprintln!(\"{m}\"); }"),
        ("a comment", "fn f() {} // Command::new(\"git\")"),
        ("a char literal quote", "fn f() { let q = '\"'; Command::new(\"gitk\"); let r = '\"'; }"),
        ("another program", "fn f() { Command::new(\"gitleaks\").status(); }"),
    ];
    for (shape, src) in mentions {
        assert!(production_spawns(src, &consts).is_empty(), "{shape} is read as a spawn:\n{src}");
    }
}
