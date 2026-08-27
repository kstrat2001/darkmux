//! (#1784, #1862) The verb index: darkmux's own command tree, walked from
//! clap at call time and rendered one line per verb, as the grounding a
//! help-shaped question is answered FROM rather than guessed AT.
//!
//! Why this exists: the answering seat was handed top-level `--help` only,
//! truncated to 1,600 characters. Every subverb and option was invisible to
//! it, so "how do I see what's loaded?" produced an invented `/machine`
//! (#1861) while `darkmux machine status` sat in the tree the whole time.
//! #1784's measurement: questions answered from data in the bundle were
//! right; questions about darkmux's own capability were wrong or lucky.
//! This makes capability a lookup.
//!
//! Compact on purpose. Subverb help in `cli.rs` is paragraphs with issue
//! archaeology (`machine status` alone is ~350 characters); the index keeps
//! the first sentence and drops parentheticals, so the whole tree fits the
//! bundle beside the catalog, config, and board. #1862 is the source-side
//! cleanup; this renderer does not wait for it.

/// One runnable invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbEntry {
    /// Space-joined path under `darkmux`, e.g. `machine status`.
    pub path: String,
    /// First sentence of the verb's help, archaeology stripped.
    pub summary: String,
    /// Long option names (`--json`), positional names in angle brackets.
    pub options: Vec<String>,
}

/// Walk the tree. Leaves only: a verb that exists just to hold subverbs is
/// not something the user runs. Hidden commands and clap's own `help` are
/// skipped.
pub fn build_verb_index(root: &clap::Command) -> Vec<VerbEntry> {
    let mut out = Vec::new();
    walk(root, &[], &mut out);
    out
}

fn walk(cmd: &clap::Command, prefix: &[&str], out: &mut Vec<VerbEntry>) {
    let subs: Vec<&clap::Command> = cmd.get_subcommands().filter(|s| !s.is_hide_set() && s.get_name() != "help").collect();
    if subs.is_empty() {
        if prefix.is_empty() {
            return; // the root itself
        }
        out.push(VerbEntry { path: prefix.join(" "), summary: summarize(cmd), options: option_names(cmd) });
        return;
    }
    for sub in subs {
        let mut p: Vec<&str> = prefix.to_vec();
        p.push(sub.get_name());
        walk(sub, &p, out);
    }
}

/// A summary never runs past this, cut at a word boundary. Help text with
/// no sentence break (an em dash, a colon, a list) would otherwise carry
/// its whole paragraph into the index.
const MAX_SUMMARY_CHARS: usize = 160;

fn summarize(cmd: &clap::Command) -> String {
    let raw = cmd
        .get_about()
        .map(|s| s.to_string())
        .or_else(|| cmd.get_long_about().map(|s| s.to_string()))
        .unwrap_or_default();
    clip_words(&first_sentence(&raw), MAX_SUMMARY_CHARS)
}

fn clip_words(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out = String::new();
    for word in text.split(' ') {
        if out.chars().count() + word.chars().count() + 1 > max_chars.saturating_sub(3) {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out.push_str("...");
    out
}

/// The first sentence, with `(...)` groups removed and whitespace
/// collapsed. A sentence ends at the first `.` followed by whitespace or the
/// end of the text, so `v1.2` and `e.g.` mid-sentence do not cut it short.
pub fn first_sentence(text: &str) -> String {
    let mut depth = 0usize;
    let mut cleaned = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => cleaned.push(ch),
            _ => {}
        }
    }
    // Removing a parenthetical leaves "ownership ." behind; close the gap.
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ").replace(" .", ".").replace(" ,", ",");
    let chars: Vec<char> = collapsed.chars().collect();
    let mut end = chars.len();
    for i in 0..chars.len() {
        if chars[i] == '.' && (i + 1 == chars.len() || chars[i + 1] == ' ') {
            // `e.g.` / `i.e.` are not sentence ends.
            let word_start = chars[..i].iter().rposition(|c| *c == ' ').map(|p| p + 1).unwrap_or(0);
            let word: String = chars[word_start..=i].iter().collect();
            if matches!(word.as_str(), "e.g." | "i.e." | "vs." | "etc.") {
                continue;
            }
            end = i + 1;
            break;
        }
    }
    chars[..end].iter().collect::<String>().trim().to_string()
}

/// How many options a line lists before an ellipsis. A verb with fifteen
/// flags is not helped by fifteen flags in the index; the seat needs the
/// shape of the invocation, and `darkmux <verb> --help` has the rest.
const MAX_OPTIONS_SHOWN: usize = 6;

fn option_names(cmd: &clap::Command) -> Vec<String> {
    let mut out = Vec::new();
    for arg in cmd.get_arguments() {
        if arg.is_hide_set() {
            continue;
        }
        let id = arg.get_id().as_str();
        if matches!(id, "help" | "version") {
            continue;
        }
        if let Some(long) = arg.get_long() {
            out.push(format!("--{long}"));
        } else if arg.is_positional() {
            out.push(format!("<{}>", arg.get_value_names().and_then(|v| v.first().map(|s| s.to_string())).unwrap_or_else(|| id.to_string())));
        }
    }
    if out.len() > MAX_OPTIONS_SHOWN {
        out.truncate(MAX_OPTIONS_SHOWN);
        out.push("...".to_string());
    }
    out
}

/// One line per verb: `darkmux <path> [<options>]: <summary>`.
pub fn render_verb_index(entries: &[VerbEntry]) -> String {
    let mut s = String::new();
    for e in entries {
        s.push_str("darkmux ");
        s.push_str(&e.path);
        if !e.options.is_empty() {
            s.push_str(" [");
            s.push_str(&e.options.join(" "));
            s.push(']');
        }
        if !e.summary.is_empty() {
            s.push_str(": ");
            s.push_str(&e.summary);
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn first_sentence_keeps_one_sentence_and_drops_parentheticals() {
        assert_eq!(
            first_sentence("Show models currently loaded in LMStudio, grouped by ownership (under the `darkmux:` namespace). Read-only. (#1426)"),
            "Show models currently loaded in LMStudio, grouped by ownership."
        );
        assert_eq!(first_sentence("Pin a value, e.g. 80, then save. Second sentence."), "Pin a value, e.g. 80, then save.");
        assert_eq!(first_sentence("No terminal period here"), "No terminal period here");
        assert_eq!(first_sentence("Multi\nline   help. More."), "Multi line help.");
        let long = "word ".repeat(60);
        let clipped = clip_words(long.trim(), 40);
        assert!(clipped.chars().count() <= 40 && clipped.ends_with("...") && !clipped.contains("wor..."), "{clipped}");
    }

    #[test]
    fn index_holds_leaf_verbs_with_their_options_and_no_help_verb() {
        let entries = build_verb_index(&crate::cli::Cli::command());
        let by_path = |p: &str| entries.iter().find(|e| e.path == p);
        let status = by_path("machine status").expect("machine status is a leaf verb");
        assert!(status.summary.starts_with("Show models currently loaded"), "{}", status.summary);
        assert!(!status.summary.contains("#1426"), "archaeology stripped: {}", status.summary);
        let list = by_path("machine list").expect("machine list");
        assert!(list.options.iter().any(|o| o == "--deep"), "{:?}", list.options);
        let eval = by_path("lab eval").expect("lab eval");
        assert!(eval.options.len() <= MAX_OPTIONS_SHOWN + 1 && eval.options.last().map(String::as_str) == Some("..."), "{:?}", eval.options);
        assert!(by_path("radio").is_some(), "top-level leaf verbs are entries too");
        assert!(by_path("machine").is_none(), "a verb that only holds subverbs is not runnable");
        assert!(entries.iter().all(|e| !e.path.split(' ').any(|w| w == "help")), "clap's help verb is not a darkmux verb");
    }

    /// The index has to fit the grounding bundle beside catalog + config +
    /// board with room to spare under the 40K hard cap. The cap in
    /// `radio_answer` is set from this measurement; if the tree grows past
    /// it, this test says so before a user sees a truncated index.
    #[test]
    fn rendered_index_fits_its_cap() {
        let rendered = render_verb_index(&build_verb_index(&crate::cli::Cli::command()));
        let n = rendered.chars().count();
        eprintln!("verb index: {} entries, {} chars", rendered.lines().count(), n);
        assert!(n <= crate::radio_answer::VERB_INDEX_CAP_CHARS, "{n} chars exceeds the cap");
        assert!(rendered.contains("darkmux machine status"), "{rendered}");
        assert!(rendered.lines().all(|l| l.chars().count() <= 260), "one line per verb, compact: {rendered}");
    }
}
