//! The `differential` fact family (v1 scope — see `darkmux-lab`'s
//! `facts.rs` for the full three-family design this is a Rust-syntax
//! sibling of): calls present in a function's pre-image that were never
//! re-added anywhere in the same region of the diff. This is the
//! best-scoped starting family for a diff-only bundler — no repo-wide
//! index required, and it's the mechanism class that has caught real
//! bugs in the TypeScript bundler's measured history (an accidentally
//! dropped validation/error-handling call survives structurally as "a
//! call disappeared," independent of what replaced it).

use crate::scan::sanitize_line;
use std::collections::HashSet;

const RUST_KEYWORDS: &[&str] = &[
    "if", "while", "for", "match", "fn", "let", "return", "impl", "struct", "enum", "trait",
    "mod", "use", "pub", "const", "static", "async", "unsafe", "where", "as", "in", "else",
    "loop", "type", "dyn", "move", "ref", "Self", "self", "super", "crate", "break", "continue",
    "true", "false", "await",
];

/// Extract call-site names from `text`: `foo(`, `x.foo(`, `Type::foo(`,
/// `foo!(` — plain function calls, method calls, path-qualified calls,
/// and macro invocations all resolve to the trailing identifier (the
/// callee/macro name), which is what a reviewer reads as "the call."
/// Keyword-shaped false positives (`if (`, the `fn foo(` declaration
/// itself) are filtered by name.
pub fn extract_calls(text: &str) -> HashSet<String> {
    let mut calls = HashSet::new();
    for line in text.lines() {
        let sanitized = sanitize_line(line);
        let chars: Vec<char> = sanitized.chars().collect();
        let mut i = 0usize;
        // Tracks the previous identifier seen (across intervening
        // whitespace/punctuation) so `fn foo(` doesn't register `foo` as
        // a call to itself — that would misfire as a false "dropped
        // call" fact if a hunk renames a function (the old name vanishes
        // from the pool, reads as a call disappearing rather than a
        // rename).
        let mut prev_ident: Option<String> = None;
        while i < chars.len() {
            if chars[i].is_alphabetic() || chars[i] == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                let mut j = i;
                while j < chars.len() && chars[j] == ' ' {
                    j += 1;
                }
                let is_call = j < chars.len() && (chars[j] == '(' || chars[j] == '!');
                let is_declaration_name = prev_ident.as_deref() == Some("fn");
                if is_call && !name.is_empty() && !is_declaration_name && !RUST_KEYWORDS.contains(&name.as_str())
                {
                    calls.insert(name.clone());
                }
                prev_ident = Some(name);
                continue;
            }
            i += 1;
        }
    }
    calls
}

/// Port of `darkmux-lab`'s `build_differential_facts`, Rust-syntax
/// sibling: calls present in `old_text` that never appear anywhere in
/// `new_text_pool` (the union of every hunk's additions in this diff —
/// a call moved to a different hunk in the SAME function still counts as
/// "not dropped").
pub fn build_differential_facts(old_text: &str, new_text_pool: &HashSet<String>) -> Vec<String> {
    if old_text.trim().is_empty() {
        return Vec::new();
    }
    let old_calls = extract_calls(old_text);
    let mut dropped: Vec<String> =
        old_calls.into_iter().filter(|n| !new_text_pool.contains(n)).collect();
    dropped.sort();
    dropped
        .into_iter()
        .map(|name| {
            format!(
                "call `{name}(...)` present in the pre-image of this region, absent from all additions in this diff"
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_calls_finds_plain_method_and_macro_forms() {
        let text = "let x = compute(1);\nself.helper();\nType::new();\nprintln!(\"hi\");";
        let calls = extract_calls(text);
        assert!(calls.contains("compute"));
        assert!(calls.contains("helper"));
        assert!(calls.contains("new"));
        assert!(calls.contains("println"));
    }

    #[test]
    fn extract_calls_ignores_keywords_and_declarations() {
        let text = "fn foo() {\n    if bar() {\n        return baz();\n    }\n}";
        let calls = extract_calls(text);
        assert!(!calls.contains("fn"));
        assert!(!calls.contains("if"));
        assert!(!calls.contains("foo"), "the declaration itself is not a call");
        assert!(calls.contains("bar"));
        assert!(calls.contains("baz"));
    }

    #[test]
    fn extract_calls_ignores_string_contents() {
        let text = r#"log("this looks_like_a_call(x) but is not");"#;
        let calls = extract_calls(text);
        assert!(!calls.contains("looks_like_a_call"));
        assert!(calls.contains("log"));
    }

    #[test]
    fn build_differential_facts_reports_only_truly_dropped_calls() {
        let old = "fn f() {\n    validate(x);\n    process(x);\n}";
        let mut pool = HashSet::new();
        pool.insert("process".to_string()); // survives, e.g. moved elsewhere in the diff
        let facts = build_differential_facts(old, &pool);
        assert_eq!(facts.len(), 1);
        assert!(facts[0].contains("`validate(...)`"));
    }

    #[test]
    fn build_differential_facts_empty_old_text_yields_nothing() {
        assert!(build_differential_facts("", &HashSet::new()).is_empty());
    }
}
