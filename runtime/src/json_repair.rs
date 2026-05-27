//! Best-effort JSON repair for truncated compactor output (#401).
//!
//! ## The empirical failure
//!
//! The 4B compactor (`darkmux:qwen3-4b-instruct-2507`) occasionally
//! enters thinking mode mid-generation and emits long runs of literal
//! `\n` escapes inside a string value (typically `active_files`).
//! These runs are technically valid JSON, but they consume the
//! compactor's per-call token budget so quickly that the model runs
//! out of room before it can:
//! 1. Terminate the open string (close `"`)
//! 2. Close the parent object/array
//! 3. Close the root object
//!
//! Result: `serde_json::from_str` fails with `"EOF while parsing a
//! string at line 1 column N"`. Empirically observed Beat 41 + Beat 46
//! + Beat 47 follow-up. Tracked as #401.
//!
//! ## What this module does
//!
//! Pure best-effort repair: walks the truncated JSON byte-by-byte
//! through a minimal state machine, then emits whatever closers are
//! needed to balance the document. The repaired output may be
//! semantically lossy (the runaway-newline content is preserved as-is
//! in the truncated string; the model's INTENDED slot content past
//! that point is permanently lost), but it parses — which means
//! downstream compaction can extract partial slot values rather than
//! bailing the whole dispatch.
//!
//! Not a general-purpose JSON repair tool; tuned for this specific
//! failure mode. A well-formed JSON document passes through unchanged.

/// (#401) Best-effort repair of a truncated JSON document.
///
/// Walks the input through a minimal state machine tracking:
/// - whether we're inside a string value
/// - whether the last character was an escape
/// - the nesting depth + the bracket type at each open level
///
/// At EOF: appends closers in reverse order — close the open string
/// (if any), then close each open container in LIFO order.
///
/// Returns the repaired string. If the input is already well-formed
/// (i.e., the state machine reaches EOF cleanly with no open string
/// + zero depth), returns the input unchanged.
///
/// **Repair limits**: this is balance-only, not a JSON synthesizer.
/// Truncation immediately after a `:` (key-value separator) or `,`
/// (item separator) produces bracket-balanced but value-missing output
/// that still fails parse — e.g., `{"a":[{"b":` → `{"a":[{"b":}]}`.
/// The brackets are balanced; the missing value is unrecoverable
/// without inventing data. `parse_with_repair` surfaces these as Err
/// from the post-repair parse attempt; downstream consumers see the
/// original "even after repair" wrapper error.
pub fn repair_truncated_json(input: &str) -> String {
    let mut in_string = false;
    let mut escaped = false;
    // Stack of open brackets — each entry is either '{' or '['.
    // We append the matching closer for each in reverse at EOF.
    let mut bracket_stack: Vec<char> = Vec::new();

    for ch in input.chars() {
        if in_string {
            if escaped {
                // The escape sequence consumed this char; reset.
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            // any other char inside string: ignore
        } else {
            match ch {
                '"' => in_string = true,
                '{' => bracket_stack.push('{'),
                '[' => bracket_stack.push('['),
                '}' => {
                    // Matching close — pop if there's an open
                    // brace. If unbalanced (close without matching
                    // open), do nothing; the appender below won't
                    // add a phantom matching opener.
                    if matches!(bracket_stack.last(), Some('{')) {
                        bracket_stack.pop();
                    }
                }
                ']' => {
                    if matches!(bracket_stack.last(), Some('[')) {
                        bracket_stack.pop();
                    }
                }
                _ => {}
            }
        }
    }

    // Well-formed case: no open string, no open brackets, no
    // trailing escape. Return input unchanged so callers can detect
    // that no repair was needed via string equality.
    if !in_string && bracket_stack.is_empty() && !escaped {
        return input.to_string();
    }

    let mut repaired = String::with_capacity(input.len() + bracket_stack.len() + 2);
    repaired.push_str(input);

    // If the input ended mid-escape (e.g., trailing `\\`), we don't
    // know what the escaped char was supposed to be — drop the
    // dangling backslash by truncating one char before appending the
    // string-terminator.
    if escaped {
        // Truncate by 1 char (the dangling backslash).
        repaired.pop();
    }

    // Close the open string (if any). The model was mid-value when
    // truncated; the unterminated portion stays in the string.
    if in_string {
        repaired.push('"');
    }

    // Close any open containers in reverse order (LIFO).
    while let Some(open) = bracket_stack.pop() {
        repaired.push(match open {
            '{' => '}',
            '[' => ']',
            _ => unreachable!("only opening braces are ever pushed onto the stack"),
        });
    }

    repaired
}

/// (#401) Try to parse a JSON document; on failure, attempt
/// best-effort repair and parse again. Returns the parsed value
/// (whichever parse succeeded) along with a `bool` indicating
/// whether repair was needed.
///
/// Repair is observability-visible in trajectory + log output via
/// the boolean flag; downstream consumers can record that a
/// repaired parse landed for forensic analysis (was the model's
/// output materially lossy? how often does this fire?).
pub fn parse_with_repair<T: serde::de::DeserializeOwned>(
    input: &str,
) -> Result<(T, bool), serde_json::Error> {
    match serde_json::from_str::<T>(input) {
        Ok(v) => Ok((v, false)),
        Err(_) => {
            let repaired = repair_truncated_json(input);
            let parsed = serde_json::from_str::<T>(&repaired)?;
            Ok((parsed, true))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct TestEnv {
        name: String,
        items: Vec<String>,
    }

    // ─── repair_truncated_json (pure) ────────────────────────────────────

    #[test]
    fn well_formed_input_passes_through_unchanged() {
        let input = r#"{"name":"test","items":["a","b"]}"#;
        assert_eq!(repair_truncated_json(input), input);
    }

    #[test]
    fn well_formed_empty_object_unchanged() {
        assert_eq!(repair_truncated_json("{}"), "{}");
    }

    #[test]
    fn well_formed_with_nested_strings_unchanged() {
        let input = r#"{"a":{"b":"c\"d","e":[1,2]}}"#;
        assert_eq!(repair_truncated_json(input), input);
    }

    #[test]
    fn truncated_mid_string_value_gets_closing_quote_and_braces() {
        // Empirical shape: model truncated mid-string with no closer.
        let input = r#"{"name":"test","items":["partial"#;
        let repaired = repair_truncated_json(input);
        // Should close string + array + object.
        let parsed: serde_json::Value = serde_json::from_str(&repaired)
            .expect("repaired input must parse");
        assert_eq!(parsed["name"], "test");
        assert_eq!(parsed["items"][0], "partial");
    }

    #[test]
    fn truncated_with_runaway_escapes_inside_string() {
        // The exact failure shape from Beat 47: thousands of literal
        // backslash-n escapes inside an active_files string. The
        // repair preserves the runaway content as-is in the
        // (now-closed) string — lossy but parseable.
        let mut input = String::from(r#"{"current_truth":{"active_files":"/workspace/x.ts"#);
        // Append 1000 backslash-n escapes (simulating the runaway).
        for _ in 0..1000 {
            input.push_str("\\n");
        }
        let repaired = repair_truncated_json(&input);
        let parsed: serde_json::Value = serde_json::from_str(&repaired)
            .expect("repaired runaway-newline JSON must parse");
        let active_files = parsed["current_truth"]["active_files"].as_str().unwrap();
        assert!(active_files.starts_with("/workspace/x.ts"));
        // The runaway newlines decode to literal newlines in the parsed value.
        assert!(active_files.len() > 1000);
    }

    #[test]
    fn truncated_mid_array_closes_array_and_outer_object() {
        let input = r#"{"items":["a","b""#;
        let repaired = repair_truncated_json(input);
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(parsed["items"][0], "a");
        assert_eq!(parsed["items"][1], "b");
    }

    #[test]
    fn truncated_just_after_opening_brace() {
        let input = r#"{"#;
        let repaired = repair_truncated_json(input);
        assert_eq!(repaired, "{}");
    }

    #[test]
    fn nested_object_truncated_after_inner_open() {
        let input = r#"{"a":{"b":{"#;
        let repaired = repair_truncated_json(input);
        // Three open braces; three closers appended.
        assert_eq!(repaired, r#"{"a":{"b":{}}}"#);
    }

    #[test]
    fn trailing_backslash_in_string_gets_truncated() {
        // Dangling escape sequence at EOF — we don't know what was
        // supposed to follow. Drop the backslash + close the string.
        let input = "{\"a\":\"hello\\";
        let repaired = repair_truncated_json(input);
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(parsed["a"], "hello");
    }

    #[test]
    fn escape_inside_string_does_not_break_state_tracking() {
        // Make sure `\"` inside a string isn't treated as a string
        // terminator.
        let input = r#"{"a":"contains \" quote","b":42}"#;
        assert_eq!(repair_truncated_json(input), input);
        let parsed: serde_json::Value = serde_json::from_str(input).unwrap();
        assert_eq!(parsed["a"], "contains \" quote");
    }

    #[test]
    fn unbalanced_close_does_not_underflow() {
        // Defensive: stray `}` at top level. The bracket stack stays
        // empty (we only pop if `last()` matches); the `}` is left in
        // place, which means the input will not parse — but we don't
        // panic. The caller catches the parse error.
        let input = "}}";
        let repaired = repair_truncated_json(input);
        // No closers added; output equals input.
        assert_eq!(repaired, "}}");
    }

    #[test]
    fn mixed_array_and_object_nesting_closed_in_correct_order() {
        // {"a": [{"b": ...
        // Need to close ... } ] } (object inside array inside object)
        let input = r#"{"a":[{"b":"#;
        let repaired = repair_truncated_json(input);
        // Last open: '{' (the inner object). Append '}' first, then ']' (array), then '}' (outer object).
        // But mid-string is false, so the bare `"b":` value is missing — invalid JSON.
        // The repair makes structure balanced but the value-missing means parse still fails.
        // That's expected: repair is best-effort, not a JSON synthesizer.
        // Just verify the bracket-balance shape.
        assert_eq!(repaired, r#"{"a":[{"b":}]}"#);
    }

    // ─── parse_with_repair (integration) ─────────────────────────────────

    #[test]
    fn parse_well_formed_returns_value_and_false() {
        let input = r#"{"name":"x","items":["a"]}"#;
        let (v, repaired) = parse_with_repair::<TestEnv>(input).unwrap();
        assert_eq!(v, TestEnv { name: "x".into(), items: vec!["a".into()] });
        assert!(!repaired, "well-formed input should not need repair");
    }

    #[test]
    fn parse_truncated_returns_value_and_true_after_repair() {
        // Truncated mid-value; repair closes string + object.
        let input = r#"{"name":"x","items":["a","partial"#;
        let (v, repaired) = parse_with_repair::<TestEnv>(input).unwrap();
        assert_eq!(v.name, "x");
        assert_eq!(v.items, vec!["a".to_string(), "partial".to_string()]);
        assert!(repaired, "truncated input should report repair=true");
    }

    #[test]
    fn parse_unrepairable_returns_err() {
        // Garbage that even after repair won't parse.
        let input = "}}";
        let result = parse_with_repair::<TestEnv>(input);
        assert!(result.is_err(), "unrepairable input must surface the parse error");
    }
}
