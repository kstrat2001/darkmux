//! (#2902 step 1b) Usage-record conformance for the runtime's model calls.
//!
//! The rule (the same one `darkmux-crew`'s `step_kinds/usage_conformance.rs`
//! holds for the host): every call darkmux makes to a model endpoint emits
//! EXACTLY ONE usage record when its reply returns. The runtime cannot write
//! flow records itself; it writes a trajectory event per call, and the host
//! tailer turns each into one `telemetry.tokens` record through the one
//! writer (`darkmux_crew::usage`):
//!
//! - an agent-loop turn: `model.completed` (`call_kind: "turn"`)
//! - a compactor call: `compaction.call` (`call_kind: "compaction"`)
//!
//! [`every_model_call_site_is_on_the_roster`] walks EVERY production source
//! file of this crate and counts each call of the HTTP transport and of the
//! client's chat methods, plus the loop's compaction entry points. A new call
//! site changes a count and fails here, so a model call cannot ship without
//! deciding its usage event. [`every_roster_path_reaches_its_event`] checks
//! that the function named as each path's recorder actually contains the
//! call that produces its event.

/// What a call site owes.
enum Duty {
    /// The HTTP transport itself; its callers owe the event.
    Transport,
    /// The reply produces one usage event per call. `records_in` is the
    /// function whose body must contain `marker` (the writer of the event,
    /// or the line that captures the call for it); `test` names the
    /// behavioral test that drives the path.
    Emits {
        records_in: (&'static str, &'static str),
        marker: &'static str,
        test: &'static str,
    },
}

struct CallSite {
    /// Path relative to this crate's manifest dir.
    file: &'static str,
    /// The function containing the call.
    caller: &'static str,
    /// The call token.
    token: &'static str,
    duty: Duty,
}

const ROSTER: &[CallSite] = &[
    CallSite {
        file: "src/lmstudio.rs",
        caller: "chat",
        token: "post",
        duty: Duty::Transport,
    },
    CallSite {
        file: "src/lmstudio.rs",
        caller: "send_streaming",
        token: "post",
        duty: Duty::Transport,
    },
    // The turn loop, non-streaming and streaming. Both replies reach
    // `run_with_sleeper`, which writes one `model.completed` per call.
    CallSite {
        file: "src/loop_runner.rs",
        caller: "run_with_sleeper",
        token: "chat",
        duty: Duty::Emits {
            records_in: ("src/loop_runner.rs", "run_with_sleeper"),
            marker: "append_model_completed(",
            test: "loop_runner::tests::every_compactor_call_lands_as_one_compaction_call_event_and_turns_name_their_model",
        },
    },
    CallSite {
        file: "src/loop_runner.rs",
        caller: "run_streaming_turn",
        token: "chat_streaming_ticking",
        duty: Duty::Emits {
            records_in: ("src/loop_runner.rs", "run_with_sleeper"),
            marker: "append_model_completed(",
            test: "lmstudio::tests::accumulated_stream_carries_the_served_model_from_its_chunks",
        },
    },
    // The two compactor call sites. Each captures its reply as a
    // `CompactorCall` the moment it returns (before any parsing that can
    // fail), so a refused compaction still accounts for every call.
    CallSite {
        file: "src/compaction.rs",
        caller: "compact",
        token: "chat",
        duty: Duty::Emits {
            records_in: ("src/compaction.rs", "compact"),
            marker: "CompactorCall::from_response(",
            test: "compaction::tests::narrative_compaction_reports_its_one_compactor_call",
        },
    },
    CallSite {
        file: "src/compaction.rs",
        caller: "call_and_parse",
        token: "chat",
        duty: Duty::Emits {
            records_in: ("src/compaction.rs", "call_and_parse"),
            marker: "CompactorCall::from_response(",
            test: "compaction::tests::structured_compaction_reports_its_one_compactor_call",
        },
    },
    // The loop's two compaction sites (resume catch-up and the main loop),
    // each for both strategies. The loop drains the captured calls into one
    // `compaction.call` trajectory event each.
    CallSite {
        file: "src/loop_runner.rs",
        caller: "run_with_sleeper",
        token: "compact",
        duty: Duty::Emits {
            records_in: ("src/loop_runner.rs", "run_with_sleeper"),
            marker: "append_compaction_call(",
            test: "loop_runner::tests::every_compactor_call_lands_as_one_compaction_call_event_and_turns_name_their_model",
        },
    },
    CallSite {
        file: "src/loop_runner.rs",
        caller: "run_with_sleeper",
        token: "compact",
        duty: Duty::Emits {
            records_in: ("src/loop_runner.rs", "run_with_sleeper"),
            marker: "append_compaction_call(",
            test: "loop_runner::tests::every_compactor_call_lands_as_one_compaction_call_event_and_turns_name_their_model",
        },
    },
    CallSite {
        file: "src/loop_runner.rs",
        caller: "run_with_sleeper",
        token: "structured_compact",
        duty: Duty::Emits {
            records_in: ("src/loop_runner.rs", "run_with_sleeper"),
            marker: "append_compaction_call(",
            test: "compaction::tests::structured_compaction_reports_its_one_compactor_call",
        },
    },
    CallSite {
        file: "src/loop_runner.rs",
        caller: "run_with_sleeper",
        token: "structured_compact",
        duty: Duty::Emits {
            records_in: ("src/loop_runner.rs", "run_with_sleeper"),
            marker: "append_compaction_call(",
            test: "compaction::tests::structured_compaction_reports_its_one_compactor_call",
        },
    },
];

/// Every name counted, as an IDENTIFIER (see [`call_sites`]): a method or
/// path call (`.chat(`, `ureq::post(`, `compaction::compact(`), a bare call
/// after a `use` (`compact(`), and a path reference that could be called
/// later (`LmStudioClient::chat` bound to a variable). `chat_streaming`
/// (test-only since #2889) and the raw `ureq` verbs are counted too, so a
/// production use of any of them has to be rostered.
const NAMES: &[&str] = &[
    "post",
    "request",
    "chat",
    "chat_streaming",
    "chat_streaming_ticking",
    "compact",
    "structured_compact",
];

/// Source files that are test-only as a whole.
fn is_test_only_file(rel: &str) -> bool {
    rel.ends_with("_tests.rs") || rel == "src/test_support.rs" || rel == "src/usage_conformance.rs"
}

/// The `{ … }` block starting at the first `{` at or after `from`, as a
/// byte range. Braces inside string, raw-string and char literals are
/// skipped (production code formats `{name}` placeholders and JSON bodies);
/// a mismatch fails loudly.
fn block_range(src: &str, from: usize) -> std::ops::Range<usize> {
    let b = src.as_bytes();
    let open = from + src[from..].find('{').expect("a block opens");
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'r' if (i == 0 || !is_ident(b[i - 1]))
                && matches!(b.get(i + 1), Some(b'"') | Some(b'#')) =>
            {
                let mut j = i + 1;
                let mut hashes = 0;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) == Some(&b'"') {
                    let close = format!("\"{}", "#".repeat(hashes));
                    let end = src[j + 1..].find(&close).expect("raw string closes");
                    i = j + 1 + end + close.len();
                    continue;
                }
            }
            b'"' => {
                let mut j = i + 1;
                while b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                i = j + 1;
                continue;
            }
            b'\'' => {
                // A char literal (`'{'`, `'\n'`, `'\''`), not a lifetime.
                if b.get(i + 1) == Some(&b'\\') {
                    let end = src[i + 3..].find('\'').expect("char literal closes");
                    i = i + 3 + end + 1;
                    continue;
                }
                if b.get(i + 2) == Some(&b'\'') {
                    i += 3;
                    continue;
                }
                let ch_len = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                if b.get(i + 1 + ch_len) == Some(&b'\'') {
                    i += 2 + ch_len;
                    continue;
                }
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return open..i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("unbalanced braces after byte {from}");
}

/// The production part of a source file: every `#[cfg(test)]` item cut out
/// (a `mod tests { … }` block, a test-only fn, a `mod x;` line), with
/// comment lines dropped and string-literal contents blanked first (a doc
/// comment or a message may mention the attribute or a call name).
fn production_source(src: &str) -> String {
    let mut prod = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    // String contents are not code: `"compact() called with no …"` in an
    // error message is not a call. Blank them (quotes and newlines kept).
    prod = blank_literals(&prod);
    while let Some(at) = prod.find("#[cfg(test)]") {
        let after = at + "#[cfg(test)]".len();
        let semi = prod[after..].find(';').map(|i| after + i);
        let brace = prod[after..].find('{').map(|i| after + i);
        let end = match (semi, brace) {
            (Some(s), Some(b)) if s < b => s + 1,
            (_, Some(b)) => block_range(&prod, b).end,
            (Some(s), None) => s + 1,
            (None, None) => prod.len(),
        };
        prod.replace_range(at..end, "");
    }
    prod
}

/// The name of the fn declared on `line`, if any.
fn declared_fn(line: &str) -> Option<&str> {
    let at = line.find("fn ")?;
    if at > 0 && !line[..at].ends_with(' ') {
        return None;
    }
    let rest = &line[at + 3..];
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    (end > 0 && rest[end..].starts_with(['(', '<'])).then(|| &rest[..end])
}

/// `src` with the contents of every string and raw-string literal replaced
/// by spaces (newlines kept, so line structure survives).
fn blank_literals(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(src.len());
    let blank = |out: &mut String, c: char| out.push(if c == '\n' { '\n' } else { ' ' });
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // Raw string: r"…", r#"…"#, br"…".
        if c == 'r' && (i == 0 || !is_ident(chars[i - 1]) || (chars[i - 1] == 'b' && (i < 2 || !is_ident(chars[i - 2])))) {
            let mut j = i + 1;
            while chars.get(j) == Some(&'#') {
                j += 1;
            }
            if chars.get(j) == Some(&'"') {
                let hashes = j - i - 1;
                out.extend(&chars[i..=j]);
                let mut k = j + 1;
                loop {
                    if chars[k] == '"' && (0..hashes).all(|h| chars.get(k + 1 + h) == Some(&'#')) {
                        break;
                    }
                    blank(&mut out, chars[k]);
                    k += 1;
                }
                out.extend(&chars[k..=k + hashes]);
                i = k + hashes + 1;
                continue;
            }
        }
        if c == '"' {
            out.push('"');
            let mut k = i + 1;
            while chars[k] != '"' {
                if chars[k] == '\\' {
                    blank(&mut out, chars[k]);
                    k += 1;
                }
                blank(&mut out, chars[k]);
                k += 1;
            }
            out.push('"');
            i = k + 1;
            continue;
        }
        if c == '\'' {
            // A char literal ('"', '\'', 'x'), not a lifetime.
            if chars.get(i + 1) == Some(&'\\') {
                let end = (i + 3..chars.len()).find(|&k| chars[k] == '\'').expect("char literal closes");
                out.push_str(&" ".repeat(end - i + 1));
                i = end + 1;
                continue;
            }
            if chars.get(i + 2) == Some(&'\'') {
                out.push_str("   ");
                i += 3;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Every production use of `name` in `prod` as a callable, by enclosing fn:
/// an occurrence at identifier boundaries that is either CALLED (`name(`)
/// or a PATH reference (`…::name`, which can be bound and called later).
/// A declaration (`fn name(`) is neither. `use` lines are skipped here;
/// [`aliases_of`] polices them.
fn call_sites(prod: &str, name: &str) -> Vec<String> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut current = String::from("<no fn>");
    let mut out = Vec::new();
    for line in prod.lines() {
        if let Some(decl) = declared_fn(line) {
            current = decl.to_string();
        }
        if line.trim_start().starts_with("use ") || line.trim_start().starts_with("pub use ") {
            continue;
        }
        for (at, _) in line.match_indices(name) {
            let before = &line[..at];
            let after = &line[at + name.len()..];
            if before.chars().next_back().is_some_and(is_ident)
                || after.chars().next().is_some_and(is_ident)
            {
                continue;
            }
            if before.trim_end().ends_with("fn") {
                continue;
            }
            if after.trim_start().starts_with('(') || before.ends_with("::") {
                out.push(current.clone());
            }
        }
    }
    out
}

/// `use` lines that import a counted name under another name, which would
/// hide its calls from [`call_sites`].
fn aliases_of(prod: &str, name: &str) -> Vec<String> {
    prod.lines()
        .filter(|l| l.trim_start().starts_with("use ") || l.trim_start().starts_with("pub use "))
        .filter(|l| l.contains(&format!("{name} as ")))
        .map(|l| l.trim().to_string())
        .collect()
}

fn crate_sources() -> Vec<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                if !is_test_only_file(&rel) {
                    out.push(rel);
                }
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    walk(&root.join("src"), root, &mut out);
    out.sort();
    out
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
        .unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// The body of production fn `name` in `file`.
fn fn_body(file: &str, name: &str) -> String {
    let prod = production_source(&read(file));
    let mut offset = 0;
    for line in prod.split_inclusive('\n') {
        if declared_fn(line) == Some(name) {
            let r = block_range(&prod, offset);
            return prod[r].to_string();
        }
        offset += line.len();
    }
    panic!("{file}: no production fn `{name}`");
}

#[test]
fn every_model_call_site_is_on_the_roster() {
    let files = crate_sources();
    assert!(
        files.iter().any(|f| f == "src/loop_runner.rs") && files.iter().any(|f| f.starts_with("src/tools/")),
        "the walk must see the whole crate: {files:?}"
    );
    for file in &files {
        let prod = production_source(&read(file));
        for token in NAMES {
            let aliases = aliases_of(&prod, token);
            assert!(
                aliases.is_empty(),
                "{file}: {aliases:?} imports `{token}` under another name, which hides its calls \
                 from this roster (#2902); call it by its own name"
            );
            let mut found = call_sites(&prod, token);
            found.sort();
            let mut rostered: Vec<String> = ROSTER
                .iter()
                .filter(|c| c.file == file && c.token == *token)
                .map(|c| c.caller.to_string())
                .collect();
            rostered.sort();
            assert_eq!(
                found, rostered,
                "{file}: production calls of `{token}` (by enclosing fn) differ from the usage \
                 roster. A new model call must decide its usage event (#2902): write one \
                 trajectory event per call that the host maps to `telemetry.tokens`, and add a \
                 `CallSite` naming its behavioral test."
            );
        }
    }
}

#[test]
fn every_roster_path_reaches_its_event() {
    for site in ROSTER {
        if let Duty::Emits { records_in: (file, func), marker, test } = site.duty {
            assert!(
                fn_body(file, func).contains(marker),
                "{}::{} calls `{}` but `{func}` in {file} never reaches `{marker}` \
                 (behavioral test: {test})",
                site.file,
                site.caller,
                site.token
            );
        }
    }
}

/// The walk's own parser, red-proven on a fixture: a `#[cfg(test)]` fn and
/// test module are cut, production code after them still counts, and a
/// declaration is not a call.
#[test]
fn production_source_cuts_test_items_but_keeps_what_follows() {
    let src = "fn a() { x.chat(r); }\n#[cfg(test)]\nfn t() { y.chat(r); }\n#[cfg(test)]\nmod tests {\n fn u() { z.chat(r); }\n}\nfn b() { w.chat(r); }\npub fn chat(&self) {}\n";
    let prod = production_source(src);
    assert_eq!(call_sites(&prod, "chat"), vec!["a".to_string(), "b".to_string()]);
}

/// The loop compacts at TWO sites (resume catch-up and the main loop), both
/// inside `run_with_sleeper`, so "the fn reaches `append_compaction_call`"
/// alone would pass with one site's drain deleted. Each site drains its own.
#[test]
fn each_loop_compaction_site_drains_its_own_calls() {
    let body = fn_body("src/loop_runner.rs", "run_with_sleeper");
    let sites = body.matches("compaction::compact(").count();
    assert_eq!(sites, 2, "the roster above names two narrative sites");
    assert_eq!(
        body.matches("trajectory.append_compaction_call(").count(),
        sites,
        "one `compaction.call` drain per compaction site (#2902)"
    );
}
