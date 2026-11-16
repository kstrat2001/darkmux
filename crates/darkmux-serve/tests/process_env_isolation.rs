//! (#3543) No test in this crate changes an environment variable that other
//! programs read (`PATH`, `HOME`, `TMPDIR`, `GIT_*`) while sharing its
//! process with other tests.
//!
//! Every child a test spawns copies this process's environment, so under
//! `cargo test` (every test of the crate a thread of one process) a test that
//! points `GIT_CONFIG_GLOBAL` at a fixture of its own reaches every other
//! test's git meanwhile, and `#[serial]` holds back only other serial tests.
//! A test that needs one of these set process-wide runs in a process of its
//! own: `darkmux_types::run_in_own_process!()` as its first statement.
//!
//! A text scan, so a lint: a function in `src/` that calls
//! `std::env::set_var`/`remove_var` and names one of these variables as a
//! string literal must run in its own process, or every function that calls
//! it must. A function calling such a helper that takes its keys as
//! arguments, and naming one of the variables, must too. A name reached only
//! through a constant defined elsewhere is not seen.

use std::path::{Path, PathBuf};

/// One function of a source file, as the scan sees it.
#[derive(Debug)]
struct Func {
    /// `name`, or `Type::name` for a method.
    callee: String,
    line: usize,
    body: String,
    /// Declared `pub` in any form: callable from other files.
    visible: bool,
}

impl Func {
    fn sets_env(&self) -> bool {
        self.body.contains("env::set_var(") || self.body.contains("env::remove_var(")
    }

    /// Whether a line of the body invokes the macro: a statement of its
    /// own, so a comment or a string naming it does not count.
    fn own_process(&self) -> bool {
        self.body.lines().map(str::trim).any(|l| {
            matches!(
                l,
                "darkmux_types::run_in_own_process!();" | "run_in_own_process!();"
            )
        })
    }

    /// Whether it sets or removes a variable whose name it is handed (not a
    /// `"…"` literal): which one is up to callers it may not see.
    fn sets_env_by_argument(&self) -> bool {
        ["env::set_var(", "env::remove_var("].iter().any(|call| {
            self.body
                .match_indices(call)
                .any(|(at, _)| !self.body[at + call.len()..].trim_start().starts_with('"'))
        })
    }

    /// Whether other files' callers decide if it reaches a child-read
    /// variable: callable from them, and setting one or a handed-in name.
    fn is_a_shared_env_helper(&self) -> bool {
        self.visible
            && self.sets_env()
            && !self.own_process()
            && (self.names_a_child_read_var() || self.sets_env_by_argument())
    }

    fn names_a_child_read_var(&self) -> bool {
        ["\"PATH\"", "\"HOME\"", "\"TMPDIR\""]
            .iter()
            .any(|v| self.body.contains(v))
            || names_a_git_var(&self.body)
    }

    fn calls(&self, callee: &str) -> bool {
        self.body
            .match_indices(&format!("{callee}("))
            .any(|(at, _)| is_a_call_at(&self.body, at))
    }
}

/// Whether `body` holds a `"GIT_…"` literal: a variable git reads.
fn names_a_git_var(body: &str) -> bool {
    body.match_indices("\"GIT_").any(|(at, _)| {
        let rest = &body[at + 5..];
        let end = rest.find(|c: char| !(c.is_ascii_uppercase() || c == '_'));
        end.is_some_and(|e| e > 0 && rest[e..].starts_with('"'))
    })
}

/// Whether the match at `at` is a call, not the start of a longer name or
/// the function's own definition.
fn is_a_call_at(body: &str, at: usize) -> bool {
    let before = &body[..at];
    let joined = before
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_');
    !joined && !before.ends_with("fn ")
}

/// `line` (already trimmed) without its `pub`, `pub(crate)`, `pub(super)`
/// or `pub(in …)` prefix; `None` when it has none.
fn strip_visibility(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("pub")?;
    let rest = match rest.strip_prefix('(') {
        Some(scoped) => scoped.split_once(')')?.1,
        None => rest,
    };
    rest.strip_prefix(' ')
}

/// The name in a `fn` line, if the line starts one.
fn fn_name(line: &str) -> Option<&str> {
    let rest = line.trim_start();
    let rest = strip_visibility(rest).unwrap_or(rest);
    let rest = rest.strip_prefix("async ").unwrap_or(rest);
    let rest = rest.strip_prefix("fn ")?;
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    Some(&rest[..end])
}

/// The type an `impl` line is for (`impl X`, `impl<T> Trait for X<T>`).
fn impl_type(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("impl")?;
    let rest = match rest.strip_prefix('<') {
        Some(generics) => generics.split_once("> ")?.1,
        None => rest.strip_prefix(' ')?,
    };
    let rest = rest.split(" for ").last()?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The last line of the item that starts at `start`: the first later line
/// that is `}` at the item's own indent (sources here are formatted).
fn item_end(lines: &[&str], start: usize) -> usize {
    let at = indent(lines[start]);
    if lines[start].trim_end().ends_with('}') {
        return start;
    }
    (start + 1..lines.len())
        .find(|&i| indent(lines[i]) == at && lines[i].trim_start().starts_with('}'))
        .unwrap_or(lines.len() - 1)
}

/// The type of the `impl` block line `i` sits in, if any.
fn enclosing_impl<'a>(lines: &[&'a str], i: usize) -> Option<&'a str> {
    let at = indent(lines[i]);
    let start = (0..i)
        .rev()
        .find(|&j| indent(lines[j]) < at && !lines[j].trim().is_empty())?;
    impl_type(lines[start]).filter(|_| item_end(lines, start) >= i)
}

/// Every function in `source`, nested ones included (an outer function's
/// body then holds its nested functions' text as well).
fn functions(source: &str) -> Vec<Func> {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(name) = fn_name(line) else { continue };
        let callee = match enclosing_impl(&lines, i) {
            Some(ty) => format!("{ty}::{name}"),
            None => name.to_string(),
        };
        let body = lines[i..=item_end(&lines, i)].join("\n");
        out.push(Func {
            callee,
            line: i + 1,
            body,
            visible: strip_visibility(line.trim_start()).is_some(),
        });
    }
    out
}

/// Each function of `funcs` that changes a variable other programs read
/// while sharing its process: `file:line callee` for each.
fn offenders(file: &str, funcs: &[Func]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut todo: Vec<&Func> = funcs
        .iter()
        .filter(|f| f.sets_env() && f.names_a_child_read_var())
        .collect();
    todo.extend(
        funcs
            .iter()
            .filter(|f| f.names_a_child_read_var() && calls_an_env_helper(f, funcs)),
    );
    // A helper other files can call has callers this file cannot see: it
    // must be private to its file, or run in its own process itself.
    out.extend(
        funcs
            .iter()
            .filter(|f| f.is_a_shared_env_helper())
            .map(|f| format!("{file}:{} {}", f.line, f.callee)),
    );
    while let Some(f) = todo.pop() {
        if f.own_process()
            || is_a_restore(f)
            || f.visible
            || !seen.insert((f.line, f.callee.clone()))
        {
            continue;
        }
        let callers: Vec<&Func> = funcs
            .iter()
            .filter(|c| c.line != f.line && c.calls(&f.callee))
            .collect();
        if callers.is_empty() {
            out.push(format!("{file}:{} {}", f.line, f.callee));
        }
        todo.extend(callers);
    }
    out
}

/// A `Drop` that puts back what its constructor set: the constructor's
/// callers are the ones checked.
fn is_a_restore(f: &Func) -> bool {
    f.callee.ends_with("::drop")
}

/// Whether `f` calls a function of `funcs` that sets variables it is handed.
fn calls_an_env_helper(f: &Func, funcs: &[Func]) -> bool {
    funcs.iter().any(|h| {
        h.line != f.line && h.sets_env() && !h.names_a_child_read_var() && f.calls(&h.callee)
    })
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_test_changes_a_variable_other_programs_read_while_sharing_its_process() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(
        files.len() > 20,
        "the scan found the crate's sources: {files:?}"
    );
    let mut found = Vec::new();
    for file in &files {
        let source = std::fs::read_to_string(file).unwrap();
        let name = file.strip_prefix(&src).unwrap().display().to_string();
        found.extend(offenders(&name, &functions(&source)));
    }
    assert!(
        found.is_empty(),
        "these change PATH, HOME, TMPDIR or a GIT_* variable, which every child of this process reads, \
         while other tests share the process; start each test that reaches them with \
         `darkmux_types::run_in_own_process!();`:\n{}",
        found.join("\n")
    );
}

// ─── The scan itself ─────────────────────────────────────────────────────

fn offenders_in(source: &str) -> Vec<String> {
    offenders("f.rs", &functions(source))
}

#[test]
fn a_test_setting_a_child_read_variable_is_flagged_unless_in_its_own_process() {
    let shared =
        "#[test]\nfn t() {\n    unsafe { std::env::set_var(\"GIT_CONFIG_GLOBAL\", \"x\") };\n}\n";
    assert_eq!(offenders_in(shared), ["f.rs:2 t"]);
    let own = shared.replace(
        "fn t() {\n",
        "fn t() {\n    darkmux_types::run_in_own_process!();\n",
    );
    assert!(offenders_in(&own).is_empty());
}

#[test]
fn only_the_macro_invoked_counts_as_its_own_process() {
    let own = |body: &str| {
        Func {
            callee: "t".into(),
            line: 1,
            body: body.into(),
            visible: false,
        }
        .own_process()
    };
    assert!(own(
        "fn t() {\n    darkmux_types::run_in_own_process!();\n}"
    ));
    assert!(own("fn t() {\n    run_in_own_process!();\n}"));
    assert!(!own(
        "fn t() {\n    // run_in_own_process! (comment only)\n}"
    ));
    assert!(!own(
        "fn t() {\n    // darkmux_types::run_in_own_process!();\n}"
    ));
    assert!(!own(
        "fn t() {\n    let s = \"darkmux_types::run_in_own_process!();\";\n}"
    ));
}

#[test]
fn an_env_setting_helper_seen_outside_its_file_is_flagged_where_it_is() {
    // Its callers in other files are out of this file's sight, so even an
    // own-process caller here does not clear it.
    let source =
        "pub(crate) fn with_path() {\n    unsafe { std::env::set_var(\"PATH\", \"x\") };\n}\n\
                  fn own() {\n    darkmux_types::run_in_own_process!();\n    with_path();\n}\n";
    assert_eq!(offenders_in(source), ["f.rs:1 with_path"]);
    let generic =
        "pub(super) fn with_env(k: &str) {\n    unsafe { std::env::set_var(k, \"x\") };\n}\n";
    assert_eq!(offenders_in(generic), ["f.rs:1 with_env"]);
    let darkmux_only = "pub(crate) fn with_secret() {\n    unsafe { std::env::set_var(\"DARKMUX_TEST_SECRET\", \"x\") };\n}\n";
    assert!(
        offenders_in(darkmux_only).is_empty(),
        "a darkmux variable is left to the env audit"
    );
}

#[test]
fn only_a_whole_literal_names_a_variable() {
    let named = |body: &str| {
        Func {
            callee: "f".into(),
            line: 1,
            body: body.into(),
            visible: false,
        }
        .names_a_child_read_var()
    };
    assert!(named("set_var(\"GIT_CONFIG_SYSTEM\", x)"));
    assert!(named("[\"HOME\", \"X\"]"));
    assert!(!named("\"GIT_\""), "a prefix alone names nothing");
    assert!(!named("\"GIT_dir\""));
    assert!(!named("\"HOMEDIR\" \"MY_PATH\""));
}

#[test]
fn a_method_is_named_by_its_type() {
    assert_eq!(
        impl_type("    impl Drop for CrewDirGuard {"),
        Some("CrewDirGuard")
    );
    assert_eq!(impl_type("impl<T: Clone> Holder<T> {"), Some("Holder"));
    assert_eq!(
        impl_type("impl<'a> From<&'a str> for Name<'a> {"),
        Some("Name")
    );
    assert_eq!(impl_type("implied();"), None);
}

#[test]
fn a_darkmux_variable_is_left_to_the_env_audit() {
    let source = "fn t() {\n    unsafe { std::env::set_var(\"DARKMUX_HOME\", \"x\") };\n}\n";
    assert!(offenders_in(source).is_empty());
}

#[test]
fn a_helper_naming_the_variable_is_checked_at_each_caller() {
    let source = "fn with_path() {\n    unsafe { std::env::set_var(\"PATH\", \"x\") };\n}\n\
                  fn own() {\n    darkmux_types::run_in_own_process!();\n    with_path();\n}\n\
                  fn shared() {\n    with_path();\n}\n";
    assert_eq!(offenders_in(source), ["f.rs:8 shared"]);
}

#[test]
fn a_helper_handed_the_variable_is_checked_where_it_is_named() {
    let source = "fn with_env(k: &str) {\n    unsafe { std::env::set_var(k, \"x\") };\n}\n\
                  fn shared() {\n    with_env(\"HOME\");\n}\n\
                  fn darkmux() {\n    with_env(\"DARKMUX_X\");\n}\n";
    assert_eq!(offenders_in(source), ["f.rs:4 shared"]);
}

#[test]
fn a_guard_is_checked_where_it_is_made_and_its_drop_is_not_a_caller() {
    let source = "struct G;\nimpl G {\n    fn new() -> Self {\n        unsafe { std::env::set_var(\"HOME\", \"x\") };\n        G\n    }\n}\n\
                  impl Drop for G {\n    fn drop(&mut self) {\n        unsafe { std::env::remove_var(\"HOME\") };\n    }\n}\n\
                  fn shared() {\n    let _g = G::new();\n}\n";
    assert_eq!(offenders_in(source), ["f.rs:13 shared"]);
}
