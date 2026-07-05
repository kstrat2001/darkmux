//! (#1060) Render a darkmux pr-reviewer dispatch into a GitHub PR-review payload.
//!
//! This is the binary-owned replacement for the per-repo `pr-review-post.py`
//! copy. The `pr-reviewer` role emits findings that each quote the line they're
//! about (`anchor`) rather than a line *number* (#1053 quote-resolve — local
//! models name the construct reliably but guess its coordinate badly). The
//! coordinate half is deterministic, so it lives here, in the binary, versioned
//! **with** the role's output schema — when the schema changes, this changes
//! with it in one release, instead of every repo updating a copied script that
//! silently degrades when it drifts.
//!
//! `darkmux pr-review render --envelope <e> --diff <d>` reads the dispatch
//! `--json` envelope + the PR diff and emits one JSON object on stdout:
//! `{ "mode": "review"|"comment", "review": <gh-review-payload>|null,
//!    "comment": <markdown>|null }`. The thin workflow YAML posts it (so the
//! operator keeps control of the model/profile, trigger, and the `gh` call);
//! `--emit <path>` writes the object to a file instead (the escape hatch).

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;

const FOOTER: &str = "\n\n---\n<sub>Automated review by darkmux's own `pr-reviewer` \
role, running on a local model (no cloud API) via a self-hosted runner — darkmux \
dogfooding itself in public. Advisory, not a merge gate.</sub>";

/// Severity → label. Unknown severity renders as a plain note.
fn sev_label(severity: Option<&str>) -> &'static str {
    match severity.map(str::to_ascii_lowercase).as_deref() {
        Some("high") => "🔴 **HIGH**",
        Some("medium") => "🟡 **MEDIUM**",
        Some("low") => "🔵 **LOW**",
        _ => "**NOTE**",
    }
}

#[derive(Debug, Deserialize)]
struct Finding {
    #[serde(default)]
    path: Option<String>,
    /// (#1053) Verbatim quote of the line this finding is about — `null` for a
    /// file-level finding. We resolve it to a line; we never trust a model line
    /// number (the old `line` field is gone).
    #[serde(default)]
    anchor: Option<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    advice: Option<String>,
    #[serde(default)]
    suggestion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReviewDoc {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    verdict: Option<String>,
    /// Tolerate `findings: null` (some local models emit it) and a missing key
    /// by coercing to `[]`, matching the Python reference's `... or []`. The
    /// no-`findings`-key → comment-fallback contract is enforced separately in
    /// `extract_review` (it checks the key is present before deserializing).
    #[serde(default, deserialize_with = "null_to_empty")]
    findings: Vec<Finding>,
}

/// Deserialize `null` (or a present list) into a `Vec<Finding>`, mapping `null`
/// to an empty vec instead of failing the whole parse.
fn null_to_empty<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Finding>, D::Error> {
    Ok(Option::<Vec<Finding>>::deserialize(d)?.unwrap_or_default())
}

/// What the verb emits: the posting mode + the payload for it.
pub struct Rendered {
    pub mode: &'static str, // "review" | "comment"
    pub review: Option<Value>,
    pub comment: Option<String>,
}

impl Rendered {
    fn to_value(&self) -> Value {
        json!({
            "mode": self.mode,
            "review": self.review.clone().unwrap_or(Value::Null),
            "comment": self.comment.clone().map(Value::String).unwrap_or(Value::Null),
        })
    }
}

/// Strip a leading `a/` `b/` `./` so a model-cited path matches the diff's path.
fn norm_path(p: &str) -> &str {
    for pre in ["a/", "b/", "./"] {
        if let Some(rest) = p.strip_prefix(pre) {
            return rest;
        }
    }
    p
}

/// Parse the `+N` new-side start out of a hunk header `@@ -a,b +c,d @@`, without
/// a regex dep: take the text after the first `+`, read its leading digits.
fn hunk_new_start(line: &str) -> Option<u32> {
    let (_, after) = line.split_once('+')?;
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Map path -> {trimmed new-side line content -> [line numbers]}. Mirrors the
/// validated Python `new_side_index`: only `+`/context lines advance the
/// new-side counter; `-`/`\` are skipped; `+++ ` is a header only before the
/// first hunk (so an added content line that is itself `+++ ...` isn't misread).
pub fn new_side_index(diff: &str) -> HashMap<String, HashMap<String, Vec<u32>>> {
    let mut out: HashMap<String, HashMap<String, Vec<u32>>> = HashMap::new();
    let (mut path, mut newln, mut in_hunk): (Option<String>, Option<u32>, bool) = (None, None, false);
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            path = None;
            newln = None;
            in_hunk = false;
        } else if !in_hunk && line.starts_with("+++ ") {
            let p = line[4..].trim();
            path = if let Some(stripped) = p.strip_prefix("b/") {
                Some(stripped.to_string())
            } else if p == "/dev/null" {
                None
            } else {
                Some(p.to_string())
            };
            if let Some(pp) = &path {
                out.entry(pp.clone()).or_default();
            }
        } else if line.starts_with("@@") {
            newln = hunk_new_start(line);
            in_hunk = true;
        } else if in_hunk {
            if let (Some(pp), Some(n)) = (path.as_ref(), newln) {
                if line.starts_with('+') || line.starts_with(' ') {
                    let content = line[1..].trim();
                    if !content.is_empty() {
                        out.get_mut(pp).unwrap().entry(content.to_string()).or_default().push(n);
                    }
                    newln = Some(n + 1);
                }
                // '-' / '\' lines have no new-side position — skip (newln unchanged).
            }
        }
    }
    out
}

/// Resolve a finding's verbatim `anchor` to a new-side line number, or `None`
/// for a file-level (`anchor` null) finding or a quote that can't be matched to
/// exactly one shown new-side line (→ general, never a guessed line). Tries the
/// quote as-is first (a line whose *content* legitimately starts with `-`/`+` —
/// a markdown bullet, a diff snippet in docs — is stored with that char intact),
/// then with one leading `+`/`-`/space stripped (a model that left the diff
/// marker on). First non-empty line of a multi-line quote; trimmed match.
pub fn resolve_anchor(
    path: Option<&str>,
    anchor: Option<&str>,
    index: &HashMap<String, HashMap<String, Vec<u32>>>,
) -> Option<u32> {
    let anchor = anchor?;
    if anchor.trim().is_empty() {
        return None;
    }
    let path = path?;
    let first = anchor.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let table = index.get(norm_path(path))?;
    let mut candidates = vec![first.trim().to_string()];
    if matches!(first.chars().next(), Some('+') | Some('-') | Some(' ')) {
        candidates.push(first[1..].trim().to_string());
    }
    for key in candidates {
        if key.is_empty() {
            continue;
        }
        if let Some(hits) = table.get(&key) {
            if hits.len() == 1 {
                return Some(hits[0]);
            }
        }
    }
    None
}

/// Pull the JSON review object out of the model's reply: prefer a fenced
/// ```json block, else the outermost `{...}`. Returns the parsed doc, or `None`
/// (unparseable / no `findings` key → caller posts a summary fallback).
fn extract_review(reply: &str) -> Option<ReviewDoc> {
    for cand in json_candidates(reply) {
        if let Ok(v) = serde_json::from_str::<Value>(cand) {
            if v.is_object() && v.get("findings").is_some() {
                if let Ok(doc) = serde_json::from_value::<ReviewDoc>(v) {
                    return Some(doc);
                }
            }
        }
    }
    None
}

/// (#1113) Freeform marker contract — the agentic reviewer's output shape.
/// A tool-granting role cannot use an `output_schema` (a grammar-constrained
/// output combined with tools makes the model skip tool-calling entirely and
/// fabricate — verified 2026-07-04), so `pr-reviewer-agentic` writes marker
/// blocks instead:
///
/// ```text
/// One-to-two-sentence summary.
///
/// MUST FIX [app/services/billing.ts] `const end = start.plus({ days: 30 })`
/// Body tracing the defect… (until the next marker or the verdict line)
///
/// VERDICT: flag
/// ```
///
/// Tried AFTER the JSON contract (structured roles are unchanged) and only
/// claims the reply when a marker or a verdict line is present — ordinary
/// prose still falls through to the comment fallback. Severity maps
/// MUST FIX → high, CONSIDER → medium; a missing verdict line derives from
/// the findings the same way the structured contract defines it (flag iff
/// any high).
fn extract_freeform_review(reply: &str) -> Option<ReviewDoc> {
    // Tolerate common model dressing on a marker line: list bullets,
    // heading hashes, bold markers.
    fn undress(line: &str) -> &str {
        let mut s = line.trim();
        for pre in ["- ", "* ", "• "] {
            if let Some(rest) = s.strip_prefix(pre) {
                s = rest.trim_start();
            }
        }
        s = s.trim_start_matches('#').trim_start();
        s.trim_start_matches("**").trim_start()
    }
    // A marker keyword must end at a word boundary — "CONSIDERED
    // alternatives…" as a body line must not phantom-split a finding
    // (review-QA finding on this PR).
    fn strip_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
        let rest = s.strip_prefix(kw)?;
        match rest.chars().next() {
            Some(c) if c.is_ascii_alphanumeric() => None,
            _ => Some(rest),
        }
    }
    fn marker_severity(line: &str) -> Option<(&'static str, &str)> {
        let s = undress(line);
        if let Some(rest) = strip_keyword(s, "MUST FIX") {
            return Some(("high", rest));
        }
        if let Some(rest) = strip_keyword(s, "CONSIDER") {
            return Some(("medium", rest));
        }
        None
    }
    fn verdict_of(line: &str) -> Option<String> {
        let s = undress(line);
        // Case-insensitive keyword ("Verdict: pass" is common English title
        // case); the first 7 bytes are only sliced when they are ASCII.
        let rest = match s.as_bytes().get(..7) {
            Some(head) if head.eq_ignore_ascii_case(b"VERDICT") => &s[7..],
            _ => return None,
        };
        let rest = rest.trim_start_matches("**");
        let rest = rest.trim_start_matches(':').trim().to_ascii_lowercase();
        if rest.starts_with("flag") {
            Some("flag".into())
        } else if rest.starts_with("pass") {
            Some("pass".into())
        } else {
            None
        }
    }
    // Marker-line remainder → ([path], `anchor`, leftover prose). The
    // leftover seeds the finding body so an inline-colon style
    // ("MUST FIX: reason on the same line") keeps its substance instead of
    // discarding it (review-QA finding on this PR).
    fn path_and_anchor(rest: &str) -> (Option<String>, Option<String>, String) {
        let path = rest.find('[').and_then(|s| {
            rest[s + 1..]
                .find(']')
                .map(|e| rest[s + 1..s + 1 + e].trim().to_string())
        });
        let (pre_bracket, after_bracket) = match (rest.find('['), rest.find(']')) {
            (Some(s), Some(e)) if s < e => (&rest[..s], &rest[e + 1..]),
            _ => ("", rest),
        };
        let (anchor, post_anchor) = match after_bracket.find('`') {
            Some(s) => match after_bracket[s + 1..].find('`') {
                Some(e) => (
                    Some(after_bracket[s + 1..s + 1 + e].to_string()),
                    format!("{}{}", &after_bracket[..s], &after_bracket[s + 2 + e..]),
                ),
                None => (None, after_bracket.to_string()),
            },
            None => (None, after_bracket.to_string()),
        };
        let leftover = format!("{pre_bracket}{post_anchor}")
            .trim_matches([':', '—', '-', '*', ' '])
            .to_string();
        (
            path.filter(|p| !p.is_empty()),
            anchor.filter(|a| !a.trim().is_empty()),
            leftover,
        )
    }

    let mut findings: Vec<Finding> = Vec::new();
    let mut summary_lines: Vec<&str> = Vec::new();
    let mut verdict: Option<String> = None;
    // (severity, path, anchor, body lines) of the block being accumulated.
    type Block = (String, Option<String>, Option<String>, Vec<String>);
    let mut current: Option<Block> = None;

    fn flush(cur: &mut Option<Block>, out: &mut Vec<Finding>) {
        if let Some((severity, path, anchor, body)) = cur.take() {
            let body: Vec<&str> = body
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            let title = body.first().map(|s| s.to_string());
            let detail = (body.len() > 1).then(|| body[1..].join("\n"));
            out.push(Finding {
                path,
                anchor,
                severity: Some(severity),
                title: title.or_else(|| Some("(finding)".into())),
                detail,
                advice: None,
                suggestion: None,
            });
        }
    }

    for line in reply.lines() {
        if let Some(v) = verdict_of(line) {
            flush(&mut current, &mut findings);
            verdict = Some(v);
        } else if let Some((sev, rest)) = marker_severity(line) {
            flush(&mut current, &mut findings);
            let (path, anchor, leftover) = path_and_anchor(rest);
            let mut body = Vec::new();
            if !leftover.is_empty() {
                body.push(leftover);
            }
            current = Some((sev.to_string(), path, anchor, body));
        } else if let Some((_, _, _, body)) = current.as_mut() {
            body.push(line.to_string());
        } else if !line.trim().is_empty() {
            summary_lines.push(line.trim());
        }
    }
    flush(&mut current, &mut findings);

    // Only claim the reply when the contract is actually present.
    if verdict.is_none() && findings.is_empty() {
        return None;
    }
    let derived = if findings.iter().any(|f| f.severity.as_deref() == Some("high")) {
        "flag"
    } else {
        "pass"
    };
    let summary = summary_lines.join(" ").trim().to_string();
    Some(ReviewDoc {
        summary: (!summary.is_empty()).then_some(summary),
        verdict: verdict.or_else(|| Some(derived.into())),
        findings,
    })
}

/// Candidate JSON slices to try, in order: a fenced ```json block, then the
/// outermost brace span.
fn json_candidates(text: &str) -> Vec<&str> {
    let mut out = vec![];
    if let Some(fence) = text.find("```") {
        let after = &text[fence + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        if let Some(end) = after.find("```") {
            let inner = after[..end].trim();
            if inner.starts_with('{') {
                out.push(inner);
            }
        }
    }
    if let (Some(s), Some(e)) = (text.find('{'), text.rfind('}')) {
        if e > s {
            out.push(&text[s..=e]);
        }
    }
    out
}

fn comment_body(f: &Finding) -> String {
    let mut parts = vec![
        format!("{} — {}", sev_label(f.severity.as_deref()), f.title.as_deref().unwrap_or("").trim()),
        String::new(),
        f.detail.as_deref().unwrap_or("").trim().to_string(),
    ];
    if let Some(a) = f.advice.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(String::new());
        parts.push(format!("**Fix:** {a}"));
    }
    // `suggestion` is the exact one-line replacement (or null) → a one-click
    // GitHub suggestion block. `advice` above is the always-present prose fix.
    if let Some(s) = f.suggestion.as_deref().filter(|s| !s.trim().is_empty()) {
        parts.push(String::new());
        parts.push("```suggestion".to_string());
        parts.push(s.trim_end_matches('\n').to_string());
        parts.push("```".to_string());
    }
    parts.join("\n").trim().to_string()
}

fn comment_fallback(reply: &str, attribution: Option<&str>) -> Rendered {
    let text = reply.trim();
    let body = if text.is_empty() {
        "_The reviewer returned no parseable output._"
    } else {
        text
    };
    Rendered {
        mode: "comment",
        review: None,
        comment: Some(format!(
            "### 🤖 darkmux PR review\n\n{body}{}",
            footer_for(attribution)
        )),
    }
}

/// Core: dispatch envelope text + diff text → the posting payload, with an
/// operator-supplied attribution line for the posted footer (#1113). The
/// default footer claims "running on a local model (no cloud API)" — true
/// for the original self-review workflow, FALSE for a dispatch routed to a
/// remote paid endpoint. The workflow that knows where the model actually
/// ran passes the truthful text; darkmux doesn't guess.
pub fn render_with_attribution(
    envelope: &str,
    diff: &str,
    attribution: Option<&str>,
) -> Rendered {
    let reply = serde_json::from_str::<Value>(envelope)
        .ok()
        .and_then(|e| e.get("final_assistant").and_then(Value::as_str).map(String::from))
        .unwrap_or_default();
    // JSON contract first (structured roles unchanged), then the freeform
    // marker contract (#1113, the agentic reviewer), else the comment
    // fallback posts the raw reply.
    let doc = match extract_review(&reply).or_else(|| extract_freeform_review(&reply)) {
        Some(d) => d,
        None => return comment_fallback(&reply, attribution),
    };

    let index = new_side_index(diff);
    let mut inline: Vec<Value> = vec![];
    let mut deferred: Vec<&Finding> = vec![];
    for f in &doc.findings {
        match resolve_anchor(f.path.as_deref(), f.anchor.as_deref(), &index) {
            Some(line) => inline.push(json!({
                "path": f.path.as_deref().map(norm_path).unwrap_or(""),
                "line": line,
                "side": "RIGHT",
                "body": comment_body(f),
            })),
            None => deferred.push(f),
        }
    }

    let summary = doc.summary.as_deref().unwrap_or("").trim();
    let verdict = doc.verdict.as_deref().unwrap_or("").trim().to_ascii_lowercase();
    // Neutral header — where the model ran is the footer attribution's job
    // (the old "(local model)" suffix here became false once a dispatch could
    // route to a remote endpoint).
    let mut body = vec!["### 🤖 darkmux PR review".to_string(), String::new()];
    if !summary.is_empty() {
        body.push(summary.to_string());
        body.push(String::new());
    }
    body.push(format!(
        "**Verdict: {}** · {} inline, {} general",
        if verdict.is_empty() { "n/a" } else { &verdict },
        inline.len(),
        deferred.len()
    ));
    if !deferred.is_empty() {
        body.push(String::new());
        body.push("**Findings not anchored to a diff line:**".to_string());
        for f in &deferred {
            let mut line = format!(
                "- {} `{}` — {}: {}",
                sev_label(f.severity.as_deref()),
                f.path.as_deref().map(norm_path).unwrap_or("?"),
                f.title.as_deref().unwrap_or("").trim(),
                f.detail.as_deref().unwrap_or("").trim()
            );
            if let Some(a) = f.advice.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                line.push_str(&format!(" _Fix: {a}_"));
            }
            body.push(line);
        }
    }

    Rendered {
        mode: "review",
        review: Some(json!({
            "event": "COMMENT",
            "body": format!("{}{}", body.join("\n"), footer_for(attribution)),
            "comments": inline,
        })),
        comment: None,
    }
}

/// The posted footer: the operator's attribution text when given (wrapped in
/// the standard rule + <sub> dressing), else the historical default.
fn footer_for(attribution: Option<&str>) -> String {
    match attribution.map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => format!("\n\n---\n<sub>{text}</sub>"),
        None => FOOTER.to_string(),
    }
}

/// `darkmux pr-review render` handler: read the envelope + diff, render, and
/// emit the `{mode, review, comment}` object to stdout (or `--emit <path>`).
pub fn cmd_render(
    envelope: &PathBuf,
    diff: &PathBuf,
    emit: Option<&PathBuf>,
    attribution: Option<&str>,
) -> Result<i32> {
    // A missing/empty envelope is the "dispatch produced nothing" case — emit a
    // comment-mode payload rather than erroring, so the workflow still posts.
    let envelope_text = std::fs::read_to_string(envelope).unwrap_or_default();
    let diff_text = std::fs::read_to_string(diff).unwrap_or_default();
    let rendered = if envelope_text.trim().is_empty() {
        comment_fallback("_The review dispatch produced no envelope._", attribution)
    } else {
        render_with_attribution(&envelope_text, &diff_text, attribution)
    };
    let out = serde_json::to_string(&rendered.to_value())?;
    match emit {
        Some(path) => std::fs::write(path, &out).with_context(|| format!("writing {}", path.display()))?,
        None => println!("{out}"),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/src/x.ts b/src/x.ts\n--- a/src/x.ts\n+++ b/src/x.ts\n@@ -1,3 +1,4 @@\n const a = 1;\n+const b = 2;\n const c = 3;\n-const d = 4;\n+const d = 5;\n";

    fn idx() -> HashMap<String, HashMap<String, Vec<u32>>> {
        new_side_index(DIFF)
    }

    #[test]
    fn index_tracks_added_and_context_excludes_removed() {
        let i = idx();
        let f = &i["src/x.ts"];
        assert_eq!(f["const b = 2;"], vec![2]); // added
        assert_eq!(f["const a = 1;"], vec![1]); // context
        assert_eq!(f["const d = 5;"], vec![4]); // added after a removed line
        assert!(!f.contains_key("const d = 4;")); // removed -> not new-side
    }

    #[test]
    fn resolve_exact_and_normalizations() {
        let i = idx();
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("const b = 2;"), &i), Some(2));
        assert_eq!(resolve_anchor(Some("b/src/x.ts"), Some("const b = 2;"), &i), Some(2)); // path prefix
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("+const b = 2;"), &i), Some(2)); // marker left on
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("   const b = 2;  "), &i), Some(2)); // whitespace
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("const b = 2;\nconst c = 3;"), &i), Some(2)); // first line
    }

    #[test]
    fn resolve_none_cases() {
        let i = idx();
        assert_eq!(resolve_anchor(Some("src/x.ts"), None, &i), None); // file-level
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("   "), &i), None); // empty
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("const z = 9;"), &i), None); // no match
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("const d = 4;"), &i), None); // removed line
    }

    #[test]
    fn resolve_ambiguous_duplicate_is_general() {
        let dup = "diff --git a/y.ts b/y.ts\n+++ b/y.ts\n@@ -1,0 +1,2 @@\n+  return;\n+  return;\n";
        let i = new_side_index(dup);
        assert_eq!(i["y.ts"]["return;"], vec![1, 2]);
        assert_eq!(resolve_anchor(Some("y.ts"), Some("return;"), &i), None);
    }

    #[test]
    fn resolve_content_starting_with_marker_as_is() {
        // markdown bullet / +-leading content: stored with the leading char, must
        // match as-is (not double-stripped). #1053 QA CONSIDER-1.
        let d = "diff --git a/doc.md b/doc.md\n+++ b/doc.md\n@@ -0,0 +1,2 @@\n+- a bullet item\n++count\n";
        let i = new_side_index(d);
        assert_eq!(resolve_anchor(Some("doc.md"), Some("- a bullet item"), &i), Some(1));
        assert_eq!(resolve_anchor(Some("doc.md"), Some("+count"), &i), Some(2));
    }

    #[test]
    fn render_resolves_anchor_to_inline_comment() {
        let envelope = json!({
            "final_assistant": "{\"summary\":\"x\",\"verdict\":\"flag\",\"findings\":[{\"path\":\"src/x.ts\",\"anchor\":\"const b = 2;\",\"severity\":\"high\",\"title\":\"T\",\"detail\":\"D\",\"advice\":\"A\",\"suggestion\":null}]}"
        })
        .to_string();
        let r = render(&envelope, DIFF);
        assert_eq!(r.mode, "review");
        let review = r.review.unwrap();
        let comments = review["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["path"], "src/x.ts");
        assert_eq!(comments[0]["line"], 2);
    }

    #[test]
    fn render_file_level_anchor_defers_to_general() {
        let envelope = json!({
            "final_assistant": "```json\n{\"summary\":\"s\",\"verdict\":\"pass\",\"findings\":[{\"path\":\"src/x.ts\",\"anchor\":null,\"severity\":\"medium\",\"title\":\"file-level\",\"detail\":\"D\",\"advice\":\"A\",\"suggestion\":null}]}\n```"
        })
        .to_string();
        let r = render(&envelope, DIFF);
        assert_eq!(r.mode, "review");
        let review = r.review.unwrap();
        assert_eq!(review["comments"].as_array().unwrap().len(), 0); // none inline
        assert!(review["body"].as_str().unwrap().contains("Findings not anchored")); // listed general
    }

    #[test]
    fn render_unparseable_falls_back_to_comment() {
        let envelope = json!({ "final_assistant": "the model rambled with no json" }).to_string();
        let r = render(&envelope, DIFF);
        assert_eq!(r.mode, "comment");
        assert!(r.comment.unwrap().contains("darkmux PR review"));
    }

    // ── new_side_index: hunk + file structure edge cases ──────────────────

    #[test]
    fn index_multi_hunk_resets_line_numbers_per_hunk() {
        let d = "diff --git a/f b/f\n+++ b/f\n@@ -1,2 +1,2 @@\n ctx1\n+add2\n@@ -10,1 +20,2 @@\n ctx20\n+add21\n";
        let f = &new_side_index(d)["f"];
        assert_eq!(f["ctx1"], vec![1]);
        assert_eq!(f["add2"], vec![2]);
        assert_eq!(f["ctx20"], vec![20]); // second hunk resets to +20
        assert_eq!(f["add21"], vec![21]);
    }

    #[test]
    fn index_multi_file_kept_separate() {
        let d = "diff --git a/one b/one\n+++ b/one\n@@ -1 +1 @@\n+alpha\ndiff --git a/two b/two\n+++ b/two\n@@ -1 +1 @@\n+beta\n";
        let i = new_side_index(d);
        assert_eq!(i["one"]["alpha"], vec![1]);
        assert_eq!(i["two"]["beta"], vec![1]);
        assert!(!i["one"].contains_key("beta"));
    }

    #[test]
    fn index_hunk_header_with_plus_in_function_context() {
        // a '+' in the trailing @@ function context must not derail the +N parse
        let d = "diff --git a/f b/f\n+++ b/f\n@@ -1,1 +1,2 @@ fn foo() + bar\n ctx\n+added\n";
        let f = &new_side_index(d)["f"];
        assert_eq!(f["ctx"], vec![1]);
        assert_eq!(f["added"], vec![2]);
    }

    #[test]
    fn index_hunk_header_without_commas() {
        let d = "diff --git a/f b/f\n+++ b/f\n@@ -5 +7 @@\n+seven\n";
        assert_eq!(new_side_index(d)["f"]["seven"], vec![7]);
    }

    #[test]
    fn index_deleted_file_has_no_new_side_entries() {
        let d = "diff --git a/gone b/gone\n--- a/gone\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-x\n-y\n";
        assert!(new_side_index(d).is_empty()); // +++ /dev/null → no path, nothing indexed
    }

    #[test]
    fn index_blank_lines_advance_counter_but_arent_indexed() {
        // "\ No newline" never advances/indexes; a blank added line advances the
        // counter (so the next line gets the right number) but isn't indexed.
        let d = "diff --git a/f b/f\n+++ b/f\n@@ -1,1 +1,3 @@\n+first\n+\n+third\n\\ No newline at end of file\n";
        let f = &new_side_index(d)["f"];
        assert_eq!(f["first"], vec![1]);
        assert_eq!(f["third"], vec![3]); // the blank at line 2 advanced the counter
        assert!(!f.contains_key("")); // blank not indexed
    }

    #[test]
    fn index_added_line_that_looks_like_a_header_is_content() {
        // an added line whose content is `+++ b/x` (full diff line `++++ b/x`)
        // inside a hunk must NOT be misread as a file header.
        let d = "diff --git a/doc.md b/doc.md\n+++ b/doc.md\n@@ -1 +1,2 @@\n+normal\n++++ b/x\n";
        let i = new_side_index(d);
        assert_eq!(i["doc.md"]["normal"], vec![1]);
        assert_eq!(i["doc.md"]["+++ b/x"], vec![2]); // content kept under doc.md
        assert!(!i.contains_key("x")); // not treated as a new file
    }

    // ── resolve_anchor: more ──────────────────────────────────────────────

    #[test]
    fn resolve_context_line_resolves() {
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("const c = 3;"), &idx()), Some(3));
    }

    #[test]
    fn resolve_path_absent_or_none_is_none() {
        let i = idx();
        assert_eq!(resolve_anchor(Some("other.ts"), Some("const b = 2;"), &i), None);
        assert_eq!(resolve_anchor(None, Some("const b = 2;"), &i), None);
    }

    #[test]
    fn resolve_marker_only_anchor_is_none() {
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("+"), &idx()), None);
    }

    #[test]
    fn resolve_multiline_skips_leading_blank_lines() {
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("\n  \nconst b = 2;"), &idx()), Some(2));
    }

    // ── extract/render: tolerant JSON extraction ──────────────────────────

    fn env(reply: &str) -> String {
        json!({ "final_assistant": reply }).to_string()
    }

    /// Test shorthand — the production entry point with the default footer.
    fn render(envelope: &str, diff: &str) -> Rendered {
        render_with_attribution(envelope, diff, None)
    }

    #[test]
    fn render_raw_json_no_fence() {
        assert_eq!(render(&env("{\"verdict\":\"pass\",\"findings\":[]}"), DIFF).mode, "review");
    }

    #[test]
    fn render_fence_without_json_tag() {
        assert_eq!(render(&env("```\n{\"verdict\":\"pass\",\"findings\":[]}\n```"), DIFF).mode, "review");
    }

    #[test]
    fn render_prose_around_json() {
        let r = render(&env("Here is my review:\n{\"verdict\":\"pass\",\"findings\":[]}\nThanks!"), DIFF);
        assert_eq!(r.mode, "review");
    }

    #[test]
    fn render_brace_inside_string_value_still_parses() {
        let r = render(&env("{\"summary\":\"use {x} here\",\"verdict\":\"pass\",\"findings\":[]}"), DIFF);
        assert_eq!(r.mode, "review");
        assert!(r.review.unwrap()["body"].as_str().unwrap().contains("use {x} here"));
    }

    #[test]
    fn render_no_findings_key_is_comment_fallback() {
        assert_eq!(render(&env("{\"summary\":\"s\",\"verdict\":\"pass\"}"), DIFF).mode, "comment");
    }

    #[test]
    fn render_empty_findings_is_pass_review() {
        let rv = render(&env("{\"verdict\":\"pass\",\"findings\":[]}"), DIFF).review.unwrap();
        assert_eq!(rv["comments"].as_array().unwrap().len(), 0);
        assert!(rv["body"].as_str().unwrap().contains("0 inline, 0 general"));
    }

    // ── render: payload shape ─────────────────────────────────────────────

    #[test]
    fn render_mixes_inline_and_deferred() {
        let reply = "{\"verdict\":\"flag\",\"findings\":[\
            {\"path\":\"src/x.ts\",\"anchor\":\"const b = 2;\",\"severity\":\"high\",\"title\":\"T1\",\"detail\":\"D1\",\"advice\":\"A1\",\"suggestion\":null},\
            {\"path\":\"src/x.ts\",\"anchor\":null,\"severity\":\"low\",\"title\":\"T2\",\"detail\":\"D2\",\"advice\":\"A2\",\"suggestion\":null}]}";
        let rv = render(&env(reply), DIFF).review.unwrap();
        assert_eq!(rv["comments"].as_array().unwrap().len(), 1); // one anchored inline
        let body = rv["body"].as_str().unwrap();
        assert!(body.contains("1 inline, 1 general"));
        assert!(body.contains("T2")); // the deferred one is listed in the summary
    }

    #[test]
    fn render_suggestion_and_fix_render_in_inline_body() {
        let reply = "{\"verdict\":\"flag\",\"findings\":[{\"path\":\"src/x.ts\",\"anchor\":\"const b = 2;\",\"severity\":\"high\",\"title\":\"T\",\"detail\":\"D\",\"advice\":\"A\",\"suggestion\":\"const b = 3;\"}]}";
        let body = render(&env(reply), DIFF).review.unwrap()["comments"][0]["body"].as_str().unwrap().to_string();
        assert!(body.contains("```suggestion"));
        assert!(body.contains("const b = 3;"));
        assert!(body.contains("**Fix:** A"));
    }

    #[test]
    fn render_normalizes_posted_path() {
        let reply = "{\"verdict\":\"flag\",\"findings\":[{\"path\":\"b/src/x.ts\",\"anchor\":\"const b = 2;\",\"severity\":\"high\",\"title\":\"T\",\"detail\":\"D\",\"advice\":\"A\",\"suggestion\":null}]}";
        assert_eq!(render(&env(reply), DIFF).review.unwrap()["comments"][0]["path"], "src/x.ts");
    }

    #[test]
    fn render_missing_verdict_is_na_and_has_footer() {
        let body = render(&env("{\"findings\":[]}"), DIFF).review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(body.contains("Verdict: n/a"));
        assert!(body.contains("dogfooding itself in public")); // FOOTER
    }

    // ── cmd_render handler ────────────────────────────────────────────────

    #[test]
    fn cmd_render_missing_envelope_emits_comment_mode() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.json");
        let diff = dir.path().join("d.diff");
        std::fs::write(&diff, DIFF).unwrap();
        cmd_render(&dir.path().join("nope.json"), &diff, Some(&out), None).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(v["mode"], "comment");
        assert!(v["comment"].as_str().unwrap().contains("no envelope"));
    }

    #[test]
    fn cmd_render_emits_review_payload_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.json");
        let envf = dir.path().join("env.json");
        let difff = dir.path().join("d.diff");
        std::fs::write(&envf, env("{\"verdict\":\"pass\",\"findings\":[]}")).unwrap();
        std::fs::write(&difff, DIFF).unwrap();
        cmd_render(&envf, &difff, Some(&out), None).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(v["mode"], "review");
        assert_eq!(v["review"]["event"], "COMMENT");
    }

    #[test]
    fn render_findings_null_coerces_to_empty_pass_review() {
        // Some local models emit `findings: null`; coerce to [] (a clean
        // review/pass), matching the Python reference — NOT a raw-JSON comment
        // fallback. (Regression caught by the port differential QA.)
        let r = render(&env("{\"verdict\":\"pass\",\"findings\":null}"), DIFF);
        assert_eq!(r.mode, "review");
        let rv = r.review.unwrap();
        assert_eq!(rv["comments"].as_array().unwrap().len(), 0);
        assert!(rv["body"].as_str().unwrap().contains("0 inline, 0 general"));
    }

    #[test]
    fn render_empty_reply_is_comment_with_no_output_note() {
        let r = render(&env(""), DIFF);
        assert_eq!(r.mode, "comment");
        assert!(r.comment.unwrap().contains("no parseable output"));
    }

    // ─── #1113: freeform marker contract (pr-reviewer-agentic) ────────

    #[test]
    fn freeform_markers_resolve_to_inline_comments() {
        let reply = "The change looks mostly sound but has one real defect.\n\
            \n\
            MUST FIX [src/x.ts] `const b = 2;`\n\
            This constant shadows the config default. Inputs passing through\n\
            the fallback path get 2 instead of the configured value.\n\
            Fix: read the default from config.\n\
            \n\
            CONSIDER [src/x.ts] `const d = 5;`\n\
            Magic number; a named constant would document intent.\n\
            \n\
            VERDICT: flag\n";
        let r = render(&env(reply), DIFF);
        assert_eq!(r.mode, "review");
        let rv = r.review.unwrap();
        let comments = rv["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 2, "both anchored findings post inline");
        assert_eq!(comments[0]["path"], "src/x.ts");
        assert_eq!(comments[0]["line"], 2); // `const b = 2;` is new-side line 2
        assert!(comments[0]["body"].as_str().unwrap().contains("HIGH"));
        assert!(comments[0]["body"]
            .as_str()
            .unwrap()
            .contains("shadows the config default"));
        assert!(comments[1]["body"].as_str().unwrap().contains("MEDIUM"));
        let body = rv["body"].as_str().unwrap();
        assert!(body.contains("Verdict: flag"), "{body}");
        assert!(body.contains("mostly sound"), "summary carried: {body}");
    }

    #[test]
    fn freeform_clean_pass_needs_only_the_verdict_line() {
        let reply = "The change achieves its stated purpose; no defects found.\n\
            \n\
            VERDICT: pass\n";
        let r = render(&env(reply), DIFF);
        assert_eq!(r.mode, "review", "a verdict line alone claims the reply");
        let rv = r.review.unwrap();
        assert_eq!(rv["comments"].as_array().unwrap().len(), 0);
        assert!(rv["body"].as_str().unwrap().contains("Verdict: pass"));
    }

    #[test]
    fn freeform_anchorless_marker_defers_to_general() {
        let reply = "Summary line.\n\
            \n\
            MUST FIX [src/x.ts]\n\
            The two halves of this change disagree about the default value —\n\
            a cross-line relationship, so no single anchor line.\n\
            \n\
            VERDICT: flag\n";
        let r = render(&env(reply), DIFF);
        let rv = r.review.unwrap();
        assert_eq!(rv["comments"].as_array().unwrap().len(), 0);
        let body = rv["body"].as_str().unwrap();
        assert!(body.contains("not anchored"), "{body}");
        assert!(body.contains("disagree about the default"), "{body}");
    }

    #[test]
    fn freeform_missing_verdict_derives_from_findings() {
        let reply = "MUST FIX [src/x.ts] `const b = 2;`\nBad constant.\n";
        let r = render(&env(reply), DIFF);
        let rv = r.review.unwrap();
        assert!(rv["body"].as_str().unwrap().contains("Verdict: flag"));
        // CONSIDER-only derives pass (mirrors the structured contract:
        // flag iff any high-severity finding).
        let reply2 = "CONSIDER [src/x.ts] `const d = 5;`\nNit.\n";
        let r2 = render(&env(reply2), DIFF);
        assert!(r2.review.unwrap()["body"]
            .as_str()
            .unwrap()
            .contains("Verdict: pass"));
    }

    #[test]
    fn freeform_plain_prose_still_falls_back_to_comment() {
        // No marker, no verdict line — the freeform layer must NOT claim it.
        let reply = "I looked at the change and it seems fine to me overall.";
        let r = render(&env(reply), DIFF);
        assert_eq!(r.mode, "comment");
    }

    #[test]
    fn freeform_json_contract_wins_over_markers() {
        // A structured reply that HAPPENS to mention "MUST FIX" in a string
        // must parse as JSON, not be re-parsed as freeform.
        let reply = "{\"summary\":\"MUST FIX mentioned in prose\",\"verdict\":\"pass\",\"findings\":[]}";
        let r = render(&env(reply), DIFF);
        let rv = r.review.unwrap();
        assert!(rv["body"].as_str().unwrap().contains("Verdict: pass"));
        assert_eq!(rv["comments"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn freeform_tolerates_bold_and_bullet_dressing() {
        let reply = "- **MUST FIX** [src/x.ts] `const b = 2;`\nBad.\n\n**VERDICT: flag**\n";
        let r = render(&env(reply), DIFF);
        let rv = r.review.unwrap();
        assert_eq!(rv["comments"].as_array().unwrap().len(), 1);
        assert!(rv["body"].as_str().unwrap().contains("Verdict: flag"));
    }

    #[test]
    fn freeform_inline_colon_marker_keeps_its_body() {
        // "MARKER: reason on the same line" is a common natural style; the
        // reason must survive as the finding body, not be discarded.
        let reply = "MUST FIX: this shadows the config default on the fallback path\n\nVERDICT: flag\n";
        let r = render(&env(reply), DIFF);
        let body = r.review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(
            body.contains("shadows the config default"),
            "inline reason must survive: {body}"
        );
    }

    #[test]
    fn freeform_verdict_is_case_insensitive() {
        let r = render(&env("Looks good overall.\n\nVerdict: pass\n"), DIFF);
        assert_eq!(r.mode, "review", "title-case Verdict must still claim the reply");
        assert!(r.review.unwrap()["body"]
            .as_str()
            .unwrap()
            .contains("Verdict: pass"));
    }

    #[test]
    fn freeform_marker_keyword_needs_a_word_boundary() {
        // A body line starting "CONSIDERED…" must not phantom-split the
        // finding, and "MUST FIXES…" at line start is not a marker.
        let reply = "MUST FIX [src/x.ts] `const b = 2;`\n\
            CONSIDERED alternatives were rejected because the default is load-bearing.\n\
            MUST FIXES of this class usually hide in the fallback path.\n\
            \n\
            VERDICT: flag\n";
        let r = render(&env(reply), DIFF);
        let rv = r.review.unwrap();
        let comments = rv["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1, "exactly one finding, not phantom-split");
        let body = comments[0]["body"].as_str().unwrap();
        assert!(body.contains("CONSIDERED alternatives"), "{body}");
        assert!(body.contains("MUST FIXES of this class"), "{body}");
    }

    // ─── #1113: footer attribution ─────────────────────────────────────

    #[test]
    fn attribution_replaces_default_footer_in_both_modes() {
        let att = "Reviewed by gpt-5.1 via Azure OpenAI on the repo's runner.";
        // Review mode.
        let r = render_with_attribution(
            &env("{\"verdict\":\"pass\",\"findings\":[]}"),
            DIFF,
            Some(att),
        );
        let body = r.review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(body.contains(att), "{body}");
        assert!(!body.contains("no cloud API"), "{body}");
        // Comment-fallback mode.
        let c = render_with_attribution(&env("just prose"), DIFF, Some(att));
        let comment = c.comment.unwrap();
        assert!(comment.contains(att));
        assert!(!comment.contains("no cloud API"));
        // Default unchanged when no attribution given.
        let d = render(&env("{\"verdict\":\"pass\",\"findings\":[]}"), DIFF);
        assert!(d.review.unwrap()["body"]
            .as_str()
            .unwrap()
            .contains("no cloud API"));
    }
}
