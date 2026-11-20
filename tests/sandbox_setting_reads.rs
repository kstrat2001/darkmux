//! Where the workspace reads `runtime.sandbox` and `runtime.tests_on_host`.
//!
//! Each unit of work (a dispatch, a lab run, a regrade, a gate step, a
//! crawl unit) reads its placement settings once and carries the value: the
//! prompt, the runtime image, the model URLs, the launch, a test run's
//! sandbox, the manifest and the in-run grader all take it, so they agree
//! by construction. Within one process the settings cannot change today
//! (`config.json` is cached once per process, and nothing in darkmux sets
//! `DARKMUX_SANDBOX`), so this does not fix a race; it keeps the agreement
//! from resting on that cache. The reads that see different values are
//! across processes: the daemon reads `config.json` per request, and a
//! regrade is its own process (it reads the settings now, and says when
//! that moves the suite from where the run's tests ran).
//!
//! darkmux-sandbox offers exactly two readers, `selection` and
//! `selection_in` (its own test derives that from its source), and the
//! types crate's accessors have unique names. So the guard is a plain token
//! match: outside those two crates' own sources, each of the names below
//! may appear only inside a named entry point, each its caller's one read.
//! A token match still sees an alias (`sb::selection`), a function value
//! (`let f = darkmux_sandbox::selection;`) and a `{self as sb}` import,
//! since each spells the name; renaming one on import (`selection as f`) is
//! refused everywhere, as is the field path spelled `runtime.sandbox` or
//! `runtime.tests_on_host` in code (a binding named `runtime`, as in
//! `if let Some(runtime) = &cfg.runtime`). Comments and string literals are
//! not code; test code (an item under `cfg(test)` or `cfg(all(test, ...))`,
//! a `_tests.rs` or `tests.rs` file, a `tests/` directory) is not scanned.
//! Every other `cfg` attribute, `cfg(not(test))` and `cfg(any(test, ...))`
//! included, may build into a shipped binary, so it is scanned.
//!
//! What it does NOT catch, stated so nobody reads more into a green run:
//! - calling an entry point (or any function that calls one, such as
//!   `RunPlacement::read` or `resolved_runtime_image`) from somewhere else:
//!   the read is inside the entry, so a second unit of work can reuse an
//!   entry to read again;
//! - an entry is matched by file and function name, so a second function of
//!   the same name in that file is allowed too, and a token after an entry
//!   function's body but before the next `fn` (a `static`, a macro) counts
//!   as inside it;
//! - the generic enum resolver `config_enum::resolve_in(setting, ..)`, which
//!   can read any enum setting given its handle;
//! - a new accessor of either setting added inside darkmux-types (its
//!   source is not scanned), until something outside calls it by a name
//!   not on the list;
//! - a raw config read through a closure or a chain
//!   (`cfg.runtime.as_ref().and_then(|r| r.sandbox)`,
//!   `cfg.runtime.as_ref()?.sandbox`), and the bare names
//!   `sandbox` / `tests_on_host`, which every variable and field so named
//!   would match.

use std::path::{Path, PathBuf};

/// The readers: darkmux-sandbox's two, and the types crate's accessors of
/// the two settings and of any enum setting.
const READERS: [&str; 8] = [
    "selection",
    "selection_in",
    "sandbox_with_source",
    "sandbox_with_source_in",
    "tests_on_host_with_source",
    "resolve_enum",
    "resolve_enum_token",
    "enum_bad_values",
];

/// The entry points that read, by file and enclosing function, each with
/// why it is its caller's one read.
const ENTRIES: [(&str, &str, &str); 16] = [
    ("crates/darkmux-crew/src/dispatch_internal.rs", "dispatch_sandbox", "a container dispatch, when its caller made no read"),
    ("crates/darkmux-crew/src/dispatch_internal.rs", "resolved_runtime_image", "a client preview's runtime image, for its Docker remedy"),
    ("crates/darkmux-crew/src/step_kinds/builtins.rs", "dispatch_opts_for", "a dispatch.internal step: its message, then its dispatch"),
    ("crates/darkmux-crew/src/step_kinds/mods_gate.rs", "run", "a mods.gate step: its kit paths and where its gate runs"),
    ("crates/darkmux-lab/src/providers/coding_task.rs", "read", "a coding-task run (RunPlacement::read): both settings, once"),
    ("crates/darkmux-lab/src/crawl/unit_step.rs", "unit_context", "a crawl unit: its paths, then its dispatch"),
    ("crates/darkmux-lab/src/providers/tool_bench.rs", "run", "a tool-bench run: its trials' dispatches and its manifest"),
    ("crates/darkmux-lab/src/lab/fixture_cli.rs", "cmd_register", "`lab fixture register`: says where verify would run now"),
    ("crates/darkmux-serve/src/client_pairing.rs", "read_preview_sandbox_at", "a preview start reads config.json fresh, per request"),
    ("crates/darkmux-serve/src/lib.rs", "sandbox_configured_now", "`/health` reads config.json fresh, per request"),
    ("crates/darkmux-serve/src/lib.rs", "health", "`/health` names the sandbox the daemon serves under"),
    ("crates/darkmux-doctor/src/checks_sandbox.rs", "configured", "doctor reports the setting as it is now, tier included"),
    ("crates/darkmux-doctor/src/checks_container.rs", "check_lab_tests_placement", "doctor reports the setting as it is now, tier included"),
    ("crates/darkmux-doctor/src/lib.rs", "check_enum_settings", "doctor reports every enum setting's bad values"),
    ("crates/darkmux-doctor/src/lib.rs", "enum_setting_pass_row", "doctor reports every enum setting as it is now"),
    ("src/main.rs", "cmd_init", "`darkmux init`'s first steps: what is configured now"),
];

/// Every hit outside an entry point, and every rename on import.
fn stray_reads(root: &Path, files: &[PathBuf]) -> Vec<String> {
    let mut out = Vec::new();
    for path in files {
        let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        let Ok(src) = std::fs::read_to_string(path) else { continue };
        let code = production(&blank_comments_and_strings(&src));
        let tokens = identifiers(&code);
        for (i, (at, word)) in tokens.iter().enumerate() {
            let line = code[..*at].matches('\n').count() + 1;
            if let Some(field) = raw_field_read(&code, &tokens, i) {
                out.push(format!("{rel}:{line}: `runtime.{field}` read as a raw config field"));
                continue;
            }
            if !READERS.contains(&word.as_str()) {
                continue;
            }
            if tokens.get(i + 1).is_some_and(|(_, w)| w == "as") {
                out.push(format!("{rel}:{line}: `{word} as` renames a reader"));
                continue;
            }
            let within = enclosing_fn(&tokens[..i]);
            let allowed = ENTRIES.iter().any(|(f, func, _)| *f == rel && Some(*func) == within);
            if !allowed {
                out.push(format!("{rel}:{line}: `{word}` in {}", within.map_or("no function".to_string(), |f| format!("fn {f}"))));
            }
        }
    }
    out
}

/// The field when the token at `i` starts `runtime.sandbox` or
/// `runtime.tests_on_host` as code (a raw read of the config struct).
fn raw_field_read<'a>(code: &str, tokens: &'a [(usize, String)], i: usize) -> Option<&'a str> {
    let ((at, word), (next_at, next)) = (&tokens[i], tokens.get(i + 1)?);
    let between = &code[at + word.len()..*next_at];
    (word == "runtime" && between.trim() == "." && matches!(next.as_str(), "sandbox" | "tests_on_host")).then_some(next.as_str())
}

/// The workspace's Rust sources outside the two crates that own the
/// settings, and outside test code by path.
fn scanned_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for top in ["crates", "src", "runtime"] {
        walk(&root.join(top), &mut out);
    }
    out.retain(|p| {
        let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        let name = p.file_name().unwrap().to_string_lossy();
        !rel.starts_with("crates/darkmux-sandbox/src/")
            && !rel.starts_with("crates/darkmux-types/src/")
            && !rel.contains("/tests/")
            && !rel.contains("/target/")
            && !name.ends_with("_tests.rs")
            && name != "tests.rs"
    });
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "target" || n == "node_modules") {
                continue;
            }
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// `src` with comments, string literals and char literals blanked to
/// spaces (newlines kept), so offsets and line numbers still hold.
fn blank_comments_and_strings(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let end = b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p);
                blank(&mut out, i, end);
                i = end;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let (mut depth, mut j) = (1, i + 2);
                while j < b.len() && depth > 0 {
                    if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                        depth += 1;
                        j += 2;
                    } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                        depth -= 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                blank(&mut out, i, j);
                i = j;
            }
            b'r' if !ident_byte(b, i) && matches!(b.get(i + 1), Some(b'"') | Some(b'#')) => {
                let hashes = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
                if b.get(i + 1 + hashes) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let close: Vec<u8> = std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
                let start = i + 2 + hashes;
                let end = b[start..].windows(close.len()).position(|w| w == close.as_slice()).map_or(b.len(), |p| start + p + close.len());
                blank(&mut out, i, end);
                i = end;
            }
            b'"' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                blank(&mut out, i, (j + 1).min(b.len()));
                i = j + 1;
            }
            b'\'' => {
                // A char literal ('x', '\n', '\u{1F600}'), not a lifetime.
                let end = if b.get(i + 1) == Some(&b'\\') {
                    b[i + 2..].iter().position(|&c| c == b'\'').map(|p| i + 2 + p + 1)
                } else if b.get(i + 2) == Some(&b'\'') {
                    Some(i + 3)
                } else {
                    None
                };
                match end {
                    Some(end) => {
                        blank(&mut out, i, end);
                        i = end;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).unwrap()
}

fn ident_byte(b: &[u8], i: usize) -> bool {
    i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')
}

/// The index of the `)` that closes a group whose `(` comes just before `s`.
fn closing_paren(s: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (k, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 1 => return Some(k),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Whether a whitespace-free `cfg` predicate holds only under test: `test`,
/// or `all(...)` with `test` among its top-level arguments.
fn test_only_predicate(pred: &str) -> bool {
    let Some(args) = pred.strip_prefix("all(").and_then(|r| r.strip_suffix(')')) else {
        return pred == "test";
    };
    let (mut depth, mut start) = (0usize, 0usize);
    let mut parts = Vec::new();
    for (k, c) in args.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&args[start..k]);
                start = k + 1;
            }
            _ => {}
        }
    }
    parts.push(&args[start..]);
    parts.contains(&"test")
}

/// The end of the `#[cfg(...)]` attribute at `at` when its item builds only
/// under test.
fn test_only_cfg_end(code: &str, at: usize) -> Option<usize> {
    let open = at + "#[cfg(".len();
    let close = open + closing_paren(&code[open..])?;
    let pred: String = code[open..close].chars().filter(|c| !c.is_whitespace()).collect();
    let end = close + code[close..].find(']')? + 1;
    test_only_predicate(&pred).then_some(end)
}

/// `code` with every item that builds only under test blanked (`cfg(test)`,
/// `cfg(all(test, ...))`): a test module, a test-only function or import.
/// Any other `cfg` attribute (`not(test)`, `any(test, ...)`) may build into
/// a shipped binary, so its item is scanned.
fn production(code: &str) -> String {
    let b = code.as_bytes();
    let mut out = b.to_vec();
    let mut from = 0;
    while let Some(p) = code[from..].find("#[cfg(") {
        let at = from + p;
        let Some(attr_end) = test_only_cfg_end(code, at) else {
            from = at + 1;
            continue;
        };
        let rest = &code[attr_end..];
        let semi = rest.find(';');
        let brace = rest.find('{');
        let end = match (semi, brace) {
            (Some(s), Some(o)) if s < o => attr_end + s + 1,
            (_, Some(o)) => {
                let mut depth = 0usize;
                let mut end = code.len();
                for (k, c) in code[attr_end + o..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = attr_end + o + k + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                end
            }
            (Some(s), None) => attr_end + s + 1,
            (None, None) => code.len(),
        };
        for c in &mut out[at..end] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
        from = end;
    }
    String::from_utf8(out).unwrap()
}

/// Every identifier in `code`, with its offset.
fn identifiers(code: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in code.char_indices().chain(std::iter::once((code.len(), ' '))) {
        let part = c.is_alphanumeric() || c == '_';
        match (start, part) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                let word = &code[s..i];
                if !word.chars().next().unwrap().is_ascii_digit() {
                    out.push((s, word.to_string()));
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// The function a token sits in: the last `fn <name>` before it.
fn enclosing_fn(before: &[(usize, String)]) -> Option<&str> {
    before.windows(2).rev().find(|w| w[0].1 == "fn").map(|w| w[1].1.as_str())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn the_placement_settings_are_read_only_at_named_entry_points() {
    let root = workspace_root();
    let files = scanned_files(&root);
    // Anti-vacuity: the walk reaches the root crate, a workspace crate and
    // the runtime, and every entry still exists where it is named.
    for expected in ["src/main.rs", "crates/darkmux-crew/src/dispatch_internal.rs", "runtime/src/lib.rs"] {
        assert!(files.iter().any(|p| p.ends_with(expected)), "the walk missed {expected}");
    }
    for (file, func, _) in ENTRIES {
        let src = std::fs::read_to_string(root.join(file)).unwrap_or_else(|_| panic!("{file}"));
        let tokens = identifiers(&production(&blank_comments_and_strings(&src)));
        let defines = tokens.windows(2).any(|w| w[0].1 == "fn" && w[1].1 == func);
        let reads = tokens.iter().enumerate().any(|(i, (_, w))| READERS.contains(&w.as_str()) && enclosing_fn(&tokens[..i]) == Some(func));
        assert!(defines && reads, "the entry {file} fn {func} no longer reads; drop it from ENTRIES");
    }
    let stray = stray_reads(&root, &files);
    assert!(stray.is_empty(), "a placement setting is read outside a named entry point:\n{}", stray.join("\n"));
}

/// The scan sees each way of spelling a read, and nothing else.
#[test]
fn the_scan_finds_a_read_however_it_is_spelled() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let case = |name: &str, body: &str| {
        let path = root.join("crates/x/src").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        stray_reads(root, &[path])
    };
    for (name, body) in [
        ("alias.rs", "use darkmux_sandbox as sb;\nfn f() { sb::selection(); }\n"),
        ("self_as.rs", "use darkmux_sandbox::{self as sb};\nfn f() { let _ = sb::selection(); }\n"),
        ("fn_value.rs", "fn f() { let g = darkmux_sandbox::selection; g(); }\n"),
        ("renamed.rs", "use darkmux_sandbox::selection as pick;\nfn f() { pick(); }\n"),
        ("types.rs", "use darkmux_types::config_access as ca;\nfn f() { ca::sandbox_with_source(); }\n"),
        ("on_host.rs", "fn f() -> bool { darkmux_types::config_access::tests_on_host_with_source().0 }\n"),
        ("enum.rs", "fn f() { let _ = darkmux_types::config_access::resolve_enum::<K>(\"runtime.sandbox\"); }\n"),
        ("raw_field.rs", "fn f(c: &Config) -> bool {\n    if let Some(runtime) = &c.runtime { runtime.tests_on_host.unwrap_or(false) } else { false }\n}\n"),
        ("not_test.rs", "#[cfg(not(test))]\nfn f() { darkmux_sandbox::selection(); }\n"),
        ("any_test.rs", "#[cfg(any(test, feature = \"x\"))]\nfn f() { darkmux_sandbox::selection(); }\n"),
    ] {
        assert!(!case(name, body).is_empty(), "{name} was not seen");
    }
    for (name, body) in [
        ("comment.rs", "// darkmux_sandbox::selection()\nfn f() {}\n"),
        ("string.rs", "fn f() { let _ = \"selection-read\"; let _ = r#\"selection\"#; }\n"),
        ("carried.rs", "fn f(s: darkmux_sandbox::Selection) { let _ = s.kind; }\n"),
        ("test_only.rs", "#[cfg(test)]\nmod tests {\n    fn f() { darkmux_sandbox::selection(); }\n}\n"),
        ("all_test.rs", "#[cfg(all(test, unix))]\nfn f() { darkmux_sandbox::selection(); }\n"),
        ("spaced_test.rs", "#[cfg( test )]\nfn f() { darkmux_sandbox::selection(); }\n"),
    ] {
        assert_eq!(case(name, body), Vec::<String>::new(), "{name}");
    }
}
