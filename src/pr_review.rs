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
//! `{ "mode": "review"|"comment"|"degraded", "review": <gh-review-payload>|null,
//!    "comment": <markdown>|null }`. The thin workflow YAML posts it (so the
//! operator keeps control of the model/profile, trigger, and the `gh` call);
//! `--emit <path>` writes the object to a file instead (the escape hatch).
//! `mode: "degraded"` (#1113) means NO review signal was produced (missing
//! envelope, empty reply, vacuous pass) — the workflow posts the comment AND
//! marks its check failed/neutral, never green.
//!
//! `darkmux pr-review run` (#1222 Phase B packet 5) is the review-FUNNEL
//! verb: it drives `darkmux_lab::lab::funnel::{run_funnel, run_judge_only}`
//! (packet 4 — bundle → probe(k draws) → dedup → double-confirm judge) using
//! the REAL bundler (`darkmux_lab::lab::bundle::{build_bundles,
//! external_bundles, slice_code}`, packet 3) and emits the same
//! `{mode, review, comment}` payload [`render_with_attribution`] does, via
//! [`synthesize_funnel`]'s three-tier synthesis: a `Tier::Confirmed` flag
//! becomes an inline, merge-blocking comment (or a general body item when
//! its anchor can't be resolved to a diff line); a `Tier::NeedsCheck` flag
//! (including a pass-2 demotion) becomes a non-blocking "worth a double
//! check" note; a `Tier::Archived` flag never renders — it stays in the
//! envelope only.

use anyhow::{anyhow, bail, Context, Result};
use darkmux_crew::dispatch::build_dispatch_record_with_payload;
use darkmux_crew::single_shot::{
    single_shot_chat, single_shot_chat_hosted, HostedSingleShotRequest, SingleShotReply,
    SingleShotRequest,
};
use darkmux_lab::lab::bundle::{
    build_bundles, external_bundles, slice_code, slice_code_probe, BundleSet, FileSource,
};
use darkmux_lab::lab::funnel::{
    run_funnel, run_judge_only, BundleInput, ChatCall, ExecMode, FunnelEmitter, FunnelEnvelope,
    FunnelInputs, JudgeRecord, LmsCycler, ProbeFlag, Tier, VerifyRecord, VerifyRuling,
};
use darkmux_profiles::crews::{resolve_crew, ResolvedCrew};
use darkmux_profiles::profiles::load_registry;
use darkmux_profiles::swap;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// (#1298) The single-reviewer (`pr-review render`) default footer. The funnel
// path never uses this — it derives its footer from the envelope's member
// provenance (see `funnel_footer`). The stale "`pr-reviewer` role" wording is
// gone from both paths; the "no cloud API" claim survives ONLY here because
// this path has no envelope to derive from and the workflow that knows where
// the model ran passes the truthful `--attribution` (the funnel does derive).
const FOOTER: &str = "\n\n---\n<sub>Automated review by darkmux, running on a \
local model (no cloud API) via a self-hosted runner — darkmux dogfooding \
itself in public. Advisory, not a merge gate.</sub>";

/// (#1298/#1186) Default tagline for the funnel's posted footer — the
/// operator-owned "why this comment exists" half. The operator overrides it
/// via `--attribution`; the model / local-vs-cloud half is DERIVED from the
/// run, never carried here (see [`dispatch_provenance`]).
const FUNNEL_TAGLINE: &str = "darkmux dogfooding itself in public. Advisory, not a merge gate.";

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
    pub mode: &'static str, // "review" | "comment" | "degraded" (#1113: no review signal)
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
    if text.is_empty() {
        return degraded_fallback("The reviewer returned no output.", attribution);
    }
    Rendered {
        mode: "comment",
        review: None,
        comment: Some(format!(
            "### 🤖 darkmux PR review\n\n{text}{}",
            footer_for(attribution)
        )),
    }
}

/// (#1113) NO review signal — distinct from both a clean pass and a prose
/// comment. A hard-killed dispatch (exit 137, zero tokens) used to render the
/// same way as "clean review, no findings": a green gate that proved nothing,
/// on a repo whose main deploys to production. `mode: "degraded"` is the
/// machine-readable half (the workflow marks its check failed/neutral instead
/// of green); the posted comment is the human-readable half. Cases: missing/
/// empty envelope, an empty reply, and a vacuous structured pass (verdict
/// pass + zero findings + no summary — the contract mandates a summary, so
/// its absence means nothing was actually reviewed).
fn degraded_fallback(note: &str, attribution: Option<&str>) -> Rendered {
    degraded_with_footer(note, &footer_for(attribution))
}

/// The degraded rendering with an explicit footer — so the funnel path can
/// pass its envelope-derived footer (#1298) while the single-reviewer path
/// keeps the static-default footer. Body text is identical either way.
fn degraded_with_footer(note: &str, footer: &str) -> Rendered {
    Rendered {
        mode: "degraded",
        review: None,
        comment: Some(format!(
            "### 🤖 darkmux PR review — ⚠️ no review signal\n\n{note} \
             **This is not a clean pass** — the automated reviewer produced \
             no usable review, so this pull request has had no automated \
             review. Human review (or a re-run) required.{footer}"
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

    // (#1113) Vacuous pass: pass-shaped (or verdict-less) doc with zero
    // findings AND no summary. The contract mandates a 1–2 sentence summary
    // on every review, so a "pass" carrying neither findings nor summary
    // proves nothing was reviewed — degrade loudly instead of rendering a
    // green-looking clean pass. A flagging doc or any real finding is real
    // signal and never degrades.
    // Pass-ish = anything that is not the contract's explicit "flag" token
    // (including absent and off-contract synonyms like "passed"/"lgtm") — a
    // zero-content review only escapes the gate by deliberately flagging
    // (review-QA finding on this PR: matching "pass" literally let a
    // pass-synonym verdict read as a green review with no content).
    let passish = doc
        .verdict
        .as_deref()
        .map(|v| !v.trim().eq_ignore_ascii_case("flag"))
        .unwrap_or(true);
    // (MSRV 1.80: map_or, not the 1.82 is_none_or)
    let no_summary = doc.summary.as_deref().map(str::trim).map_or(true, str::is_empty);
    if doc.findings.is_empty() && passish && no_summary {
        return degraded_fallback(
            "The reviewer emitted an empty pass: no findings and no summary.",
            attribution,
        );
    }

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

/// (#1298) The funnel's posted footer, with its dispatch-provenance clause
/// DERIVED from the envelope's member records — never a hardcoded claim that
/// can contradict the run. The prior static footer asserted "`pr-reviewer`
/// role, running on a local model (no cloud API)" unconditionally; on the
/// first all-remote (Azure) funnel review that was three lies in the most
/// visible place — a public comment saying "no cloud API" about a cloud
/// review (#1186 "never off the meter"). The operator's `--attribution`
/// supplies only the tagline; WHERE the models ran is computed from what
/// actually ran.
fn funnel_footer(env: &FunnelEnvelope, attribution: Option<&str>) -> String {
    let tagline = attribution
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(FUNNEL_TAGLINE);
    format!(
        "\n\n---\n<sub>Automated review by darkmux's review funnel, {} — {tagline}</sub>",
        dispatch_provenance(env)
    )
}

/// (#1298) The dispatch-provenance clause: WHERE the funnel's seats actually
/// ran, read from the envelope's member records. An audit-integrity surface —
/// a posted review must never claim "no cloud API" about a review that
/// dispatched to a hosted endpoint.
///
/// - All seats local → "on a local model, no cloud API" (the only honest home
///   for that phrase).
/// - Any seat remote → names the hosted model(s); never "no cloud API".
/// - Mixed crew → names both the local and the hosted seat models.
/// - No member records → asserts neither local nor cloud (stays honest).
fn dispatch_provenance(env: &FunnelEnvelope) -> String {
    let local = unique_seat_models(env, false);
    let remote = unique_seat_models(env, true);
    match (local.is_empty(), remote.is_empty()) {
        (true, true) => "on a self-hosted runner".to_string(),
        (false, true) => format!("on a local model, no cloud API ({})", local.join(", ")),
        (true, false) => format!("via a hosted cloud endpoint ({})", remote.join(", ")),
        (false, false) => format!(
            "on a mixed crew (local: {}; hosted cloud endpoint: {})",
            local.join(", "),
            remote.join(", ")
        ),
    }
}

/// (#1298) Distinct seat model names filtered by remote-ness, first-seen order
/// preserved. (#1300) The name comes from `member.model` (the dispatched model
/// id) today; when #1300 lands — capturing the endpoint's SERVED model into
/// the member record — prefer that field here (a one-line swap of `name`).
fn unique_seat_models(env: &FunnelEnvelope, remote: bool) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for m in env.members.iter().filter(|m| m.remote == remote) {
        // (#1300) prefer the served model over `m.model` when it lands.
        let name = m.model.as_str();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
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
        degraded_fallback("The review dispatch produced no envelope.", attribution)
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

// ═══════════════════════════════════════════════════════════════════════
// `darkmux pr-review run` — the review funnel verb (#1222 Phase B packet 5)
// ═══════════════════════════════════════════════════════════════════════

/// Parsed `darkmux pr-review run` CLI surface. Mirrors the clap variant's
/// field names 1:1 so `main.rs`'s dispatch arm is a plain struct-literal
/// shorthand (see `PrReviewCmd::Render` -> `cmd_render` for the same
/// pattern one level down).
pub struct RunOpts {
    /// GitHub source `owner/repo` — bundles via the GitHub API, no local
    /// checkout. Requires `head_sha`; mutually exclusive with `worktree`
    /// (clap enforces both).
    pub github: Option<String>,
    pub head_sha: Option<String>,
    /// Local worktree directory — bundles via a real checkout. Alternative
    /// to `github`/`head_sha`.
    pub worktree: Option<PathBuf>,
    pub diff: PathBuf,
    /// The author's stated intent for the change, fed into the JUDGE prompt
    /// ONLY — Phase A never showed the probe seat the intent, so
    /// `funnel::probe_user_message` never reads it either (#1256). This CLI
    /// carries one blob (no separate title field), passed through as
    /// `FunnelInputs::intent_body` with `intent_title` empty; an absent or
    /// empty file -> "(no description provided)" (matches
    /// `funnel::judge_prompt`'s own per-field fallback).
    pub intent_file: Option<PathBuf>,
    /// Crew name from `profiles.json`'s `"crews"` map, staffing the
    /// `review-probe`/`review-judge` seats. Required unless `from_envelope`
    /// is set (synthesis-only needs no crew).
    pub crew: Option<String>,
    /// `"sequential"` | `"parallel"` | `"auto"`.
    pub mode: String,
    /// Overrides every `review-probe` staffing's draw count (`k`).
    pub k: Option<u32>,
    /// Write the full funnel envelope (pretty JSON) here.
    pub envelope_out: Option<PathBuf>,
    /// Write the rendered `{mode, review, comment}` payload here. `None`,
    /// or the literal path `-`, writes to stdout (mirrors `cmd_render`'s
    /// `None` == stdout convention, plus the explicit `-` spelling the CI
    /// path uses).
    pub emit: Option<PathBuf>,
    pub timeout: u32,
    pub profiles: Option<String>,
    pub attribution: Option<String>,
    /// External bundler command (`<cmd> --worktree <dir> --diff <file>`),
    /// standing in for the built-in `build_bundles`.
    pub bundler: Option<String>,
    /// Re-judge a previously-recorded flag list without re-running the
    /// probe (`run_judge_only` — the bundler still runs, since the judge
    /// needs the code each flag's `bundle_id` refers to).
    pub charges_file: Option<PathBuf>,
    /// Synthesis-only: read a previously-written `--envelope-out` and emit
    /// the rendered payload — zero model calls, zero bundling. The
    /// CI-testable path.
    pub from_envelope: Option<PathBuf>,
}

/// `darkmux pr-review run` handler: drive the review funnel end-to-end (or
/// synthesis-only, via `--from-envelope`) and emit the same
/// `{mode, review, comment}` payload `render`/`cmd_render` do.
pub fn cmd_run(opts: RunOpts) -> Result<i32> {
    let diff_text = std::fs::read_to_string(&opts.diff)
        .with_context(|| format!("reading --diff {}", opts.diff.display()))?;

    let env: FunnelEnvelope = if let Some(path) = &opts.from_envelope {
        // Synthesis-only: dispatch-shaping flags have nothing to shape.
        // Warn (don't error) so a copy-pasted full command still runs —
        // operator sovereignty: surface, never silently ignore.
        let mut ignored: Vec<&str> = Vec::new();
        if opts.crew.is_some() {
            ignored.push("--crew");
        }
        if opts.worktree.is_some() {
            ignored.push("--worktree");
        }
        if opts.github.is_some() {
            ignored.push("--github/--head-sha");
        }
        if opts.bundler.is_some() {
            ignored.push("--bundler");
        }
        if opts.k.is_some() {
            ignored.push("--k");
        }
        if !ignored.is_empty() {
            eprintln!(
                "darkmux pr-review run: {} ignored with --from-envelope (synthesis-only, no dispatch)",
                ignored.join(", ")
            );
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading --from-envelope {}", path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("parsing --from-envelope {} as a funnel envelope", path.display()))?
    } else {
        run_dispatch(&opts, &diff_text)?
    };

    if let Some(path) = &opts.envelope_out {
        let pretty = serde_json::to_string_pretty(&env).context("serializing the funnel envelope")?;
        std::fs::write(path, pretty)
            .with_context(|| format!("writing --envelope-out {}", path.display()))?;
    }

    let rendered = synthesize_funnel(&env, &diff_text, opts.attribution.as_deref());
    emit_rendered(&rendered, opts.emit.as_deref())
}

/// `--emit <path>` writes the rendered payload to a file; `--emit -` or a
/// bare omitted `--emit` writes to stdout (the `-` spelling is what makes
/// the CI-testable `--from-envelope ... --emit -` path assertable without
/// a scratch file).
fn emit_rendered(rendered: &Rendered, emit: Option<&Path>) -> Result<i32> {
    let out = serde_json::to_string(&rendered.to_value())?;
    match emit {
        Some(p) if p == Path::new("-") => println!("{out}"),
        Some(p) => std::fs::write(p, &out).with_context(|| format!("writing {}", p.display()))?,
        None => println!("{out}"),
    }
    Ok(0)
}

/// The worktree/GithubApi source, or a loud named error — never a silent
/// "which source?" guess. The clap `conflicts_with`/`requires` pairing on
/// `RunOpts`'s CLI surface already rules out both-set and github-without-
/// head-sha; the arms here are defense-in-depth for any caller that builds
/// `RunOpts` directly (e.g. a future library consumer bypassing clap).
fn resolve_source(opts: &RunOpts) -> Result<FileSource> {
    match (&opts.worktree, &opts.github, &opts.head_sha) {
        (Some(w), None, _) => Ok(FileSource::worktree(w)),
        (None, Some(repo), Some(sha)) => Ok(FileSource::github_api(repo.clone(), sha.clone())),
        (None, Some(_), None) => bail!("darkmux pr-review run: --github requires --head-sha"),
        (None, None, _) => bail!(
            "darkmux pr-review run: pass either --worktree <dir> or --github <owner/repo> \
             --head-sha <sha> to source the bundler (or --from-envelope for synthesis-only, \
             which needs neither)"
        ),
        (Some(_), Some(_), _) => {
            bail!("darkmux pr-review run: --worktree and --github are mutually exclusive")
        }
    }
}

fn parse_exec_mode(mode: &str) -> Result<ExecMode> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "sequential" => Ok(ExecMode::Sequential),
        "parallel" => Ok(ExecMode::Parallel),
        "auto" => Ok(ExecMode::Auto),
        other => bail!(
            "darkmux pr-review run: --mode must be one of sequential|parallel|auto (got \"{other}\")"
        ),
    }
}

/// `Bundle` (packet 3's `darkmux_lab::lab::bundle`) -> `BundleInput`
/// (packet 4's `darkmux_lab::lab::funnel` shape) — the reconciliation the
/// funnel module's own doc names as this packet's job. Each bundle's
/// line-span pointers are rendered PER SEAT (#1256): `slice_code` (the
/// judge's `// path` raw format) into `code`, `slice_code_probe` (the
/// probe's Phase A ``### `path`` + ```` ```typescript ```` fenced format)
/// into `probe_code`.
fn bundle_inputs_from_set(set: &BundleSet, source: &FileSource) -> Result<Vec<BundleInput>> {
    set.bundles
        .iter()
        .map(|b| {
            let code = slice_code(source, &b.code)
                .with_context(|| format!("slicing code for bundle \"{}\"", b.id))?;
            let probe_code = slice_code_probe(source, &b.code)
                .with_context(|| format!("probe-slicing code for bundle \"{}\"", b.id))?;
            Ok(BundleInput {
                id: b.id.clone(),
                fact_family: b.fact_family.clone(),
                code,
                probe_code,
                facts: b.facts.clone(),
                manifest: b.manifest.clone(),
            })
        })
        .collect()
}

/// Real `Bundle.id`s are `"<fn>@<path>"` (packet 3's `build_bundles`); the
/// provisional funnel-internal bundler used `id == path` with no `@`.
/// `split_once('@')` — the FIRST `@` — handles both: function names never
/// contain `@`, but paths can (an npm `@scope/pkg` vendored path), so
/// splitting on the LAST `@` would mangle them. Falls back to the whole id
/// when there's no `@` to split on.
fn path_from_bundle_id(bundle_id: &str) -> &str {
    bundle_id.split_once('@').map(|(_, p)| p).unwrap_or(bundle_id)
}

/// (#1247 Part 1) Production wiring of `funnel::FunnelEmitter` — writes
/// through the real darkmux-flow machinery (`darkmux_flow::record`), the
/// same engagement-scoped stream `crew dispatch`/`sprint review` write
/// through (env/config-resolved sink, `machine_id`/`orchestrator`
/// auto-stamped at write time). This is the FLEET sink: `darkmux pr-review
/// run` drives ONE case per invocation (a real PR review), so its run/step/
/// ruling records belong on the operator's real engagement stream. Contrast
/// `darkmux lab review-bench --funnel`'s per-run-local JSONL sink
/// (`LocalJsonlEmitter` in `darkmux_lab::lab::review_bench`) — a bench run
/// dispatches many cases in one process and must never spam that volume
/// into the fleet stream (lab-vs-fleet scope boundary). Failure to record
/// is swallowed (`let _ =`) — same discipline as every other flow emit site
/// in the codebase; a flow-record write failure must never abort a review.
struct FleetFlowEmitter;

impl FunnelEmitter for FleetFlowEmitter {
    fn emit(&mut self, record: darkmux_flow::FlowRecord) {
        let _ = darkmux_flow::record(record);
    }
}

/// (#1272) Every distinct model this crew's resolved seats will dispatch,
/// darkmux-namespaced (`swap::namespaced_identifier` — the same form
/// `FunnelEnvelope::staffing` records) and deduped/sorted for a stable
/// display string. A funnel run is inherently multi-model (>=1 probe seat
/// plus the judge seat) — unlike a single-model `crew dispatch`, there's no
/// one "the model" for the dispatch bookend's `model` field, so this is the
/// closest honest equivalent: every model actually in play, comma-joined,
/// rather than a single representative pick. `None` only if the crew has no
/// staffed seats at all (a case `run_dispatch`'s own validation already
/// rejects before this ever runs).
fn crew_model_summary(crew: &ResolvedCrew) -> Option<String> {
    // (#1260) Remote seats keep the profile's bare model id — nothing is
    // loaded in LMStudio, so no darkmux-namespaced identifier exists for
    // them; local seats keep the namespaced form.
    let mut ids: Vec<String> = crew
        .seats
        .values()
        .flatten()
        .map(|s| {
            if s.pm.is_remote() {
                s.pm.id.clone()
            } else {
                swap::namespaced_identifier(&s.pm)
            }
        })
        .collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        None
    } else {
        Some(ids.join(", "))
    }
}

/// (#1272) One funnel-run dispatch bookend record. Same builder + field
/// shape `crew dispatch` uses (`build_dispatch_record_with_payload`), with
/// ONE deliberate difference: `source` is overridden from the builder's
/// hardcoded `"crew_dispatch"` to `"funnel"` — the SAME provenance tag
/// every sibling `funnel.task/step/ruling` record in this session carries
/// (`funnel_flow_record` in `darkmux_lab::lab::funnel`). One session, one
/// source: without the override, the viewer's Source facet would split a
/// single production review's records across two contradictory provenance
/// tags, and the mislabel would persist durably in the flow/audit files.
fn funnel_bookend_record(
    level: darkmux_flow::Level,
    action: &str,
    crew_name: &str,
    case_id: &str,
    model: Option<&str>,
    payload: serde_json::Value,
) -> darkmux_flow::FlowRecord {
    let mut record = build_dispatch_record_with_payload(
        level,
        action,
        crew_name,
        case_id,
        model,
        None,
        None,
        Some(payload),
    );
    record.source = Some("funnel".to_string());
    record
}

/// (#1272) Panic/early-return backstop for [`with_dispatch_bookends`] —
/// same RAII shape as `darkmux-crew`'s `DispatchBookendGuard` (#717): once
/// armed, a `Drop` while still armed means the wrapped call unwound (a
/// panic) before either terminal branch in `with_dispatch_bookends` ran, so
/// this fires a `dispatch error` terminal itself. The normal Ok/Err paths
/// disarm before emitting their own terminal record, so a clean run is
/// never double-counted — matching `DispatchBookendGuard`'s own discipline.
struct FunnelDispatchBookendGuard<'a> {
    armed: bool,
    emitter: &'a mut dyn FunnelEmitter,
    case_id: String,
    crew_name: String,
    model: Option<String>,
}

impl Drop for FunnelDispatchBookendGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.emitter.emit(funnel_bookend_record(
            darkmux_flow::Level::Error,
            "dispatch error",
            &self.crew_name,
            &self.case_id,
            self.model.as_deref(),
            json!({
                "runtime": "funnel",
                "result_class": "error",
                "error": "funnel dispatch terminated before completion (early return or panic)",
            }),
        ));
    }
}

/// (#1272) Emit `dispatch start`, run `f`, then emit the matching terminal
/// record — `dispatch complete` on `Ok`, `dispatch error` (carrying the
/// real error text) on `Err`. Same shape `crew dispatch` emits around every
/// dispatch ([`funnel_bookend_record`] — the `crew dispatch` builder with
/// only `source` re-tagged `"funnel"`; action = `"dispatch start"`/
/// `"dispatch complete"`/`"dispatch error"`, no new action vocabulary), so
/// a production `pr-review run` opens/closes the SAME liveness edge the
/// viewer's fleet/machine surfaces already key on (#857 — a session's open
/// edge is its `dispatch.start` record, per `darkmux-flow`'s
/// `presence_reconciler`) that `crew dispatch` opens today. Before this fix, the funnel's OWN `funnel.task/step/ruling`
/// vocabulary (#1247/#1248) was the only thing a production `pr-review
/// run` ever emitted, so a real production review never showed up as a
/// running dispatch anywhere in the viewer (#1272).
///
/// `f` (the funnel dispatch itself — `run_funnel` or `run_judge_only`) is
/// injected so a test can drive a scripted stand-in instead of a real crew
/// and LMStudio round trip, and receives the SAME [`FunnelEmitter`] the
/// bookend records go through (production: [`FleetFlowEmitter`]) — one
/// sink for the whole `pr-review run` invocation, so `dispatch start`
/// always precedes every `funnel.*` record and the terminal record always
/// follows them, in the SAME emitter's observed sequence.
///
/// Terminal-on-all-paths: see [`FunnelDispatchBookendGuard`] for the
/// panic backstop; the `?`-return case is simply `f` returning `Err`,
/// handled by the `Err` arm below like any other early exit.
fn with_dispatch_bookends(
    emitter: &mut dyn FunnelEmitter,
    case_id: &str,
    crew_name: &str,
    model: Option<&str>,
    f: impl FnOnce(&mut dyn FunnelEmitter) -> Result<FunnelEnvelope>,
) -> Result<FunnelEnvelope> {
    emitter.emit(funnel_bookend_record(
        darkmux_flow::Level::Info,
        "dispatch start",
        crew_name,
        case_id,
        model,
        json!({ "runtime": "funnel" }),
    ));
    let mut guard = FunnelDispatchBookendGuard {
        armed: true,
        emitter,
        case_id: case_id.to_string(),
        crew_name: crew_name.to_string(),
        model: model.map(str::to_string),
    };
    let result = f(&mut *guard.emitter);
    // The wrapped call returned (rather than unwinding) — disarm the panic
    // backstop before emitting our own terminal record below, so a clean
    // Ok/Err return is never double-counted by `FunnelDispatchBookendGuard`'s
    // Drop (same discipline as `DispatchBookendGuard::disarm`).
    guard.armed = false;
    match result {
        Ok(env) => {
            let mut payload = json!({ "runtime": "funnel", "result_class": "ok" });
            // (#1260/#1186) Remote-seat tokens ride the dispatch bookend too,
            // so downstream token-accounting surfaces can EXCLUDE cloud
            // tokens from local-savings math without opening the envelope.
            if env.members.iter().any(|m| m.remote) {
                let remote_tokens: u64 =
                    env.members.iter().filter(|m| m.remote).map(|m| m.total_tokens).sum();
                payload["remote_tokens"] = remote_tokens.into();
            }
            guard.emitter.emit(funnel_bookend_record(
                darkmux_flow::Level::Info,
                "dispatch complete",
                crew_name,
                case_id,
                model,
                payload,
            ));
            Ok(env)
        }
        Err(e) => {
            guard.emitter.emit(funnel_bookend_record(
                darkmux_flow::Level::Error,
                "dispatch error",
                crew_name,
                case_id,
                model,
                json!({
                    "runtime": "funnel",
                    "result_class": "error",
                    "error": e.to_string(),
                }),
            ));
            Err(e)
        }
    }
}

/// Everything but `--from-envelope`: resolve the source + crew, build real
/// bundles, and dispatch either `run_funnel` (the full pipeline) or
/// `run_judge_only` (`--charges-file` — re-judge a saved flag list without
/// re-running the probe; the bundler still runs since the judge needs the
/// code each flag's `bundle_id` refers to).
fn run_dispatch(opts: &RunOpts, diff_text: &str) -> Result<FunnelEnvelope> {
    let crew_name = match opts.crew.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) => c.to_string(),
        None => {
            let available = load_registry(opts.profiles.as_deref())
                .map(|l| {
                    l.registry
                        .crews
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            bail!(
                "darkmux pr-review run: --crew is required (unless --from-envelope) — name a \
                 crew from your profiles.json's \"crews\" map. Available: {}",
                if available.is_empty() { "(none)".to_string() } else { available }
            );
        }
    };

    let source = resolve_source(opts)?;
    let loaded = load_registry(opts.profiles.as_deref())?;
    let mut crew = resolve_crew(&loaded.registry, &crew_name)?;
    if let Some(k) = opts.k {
        if let Some(staffings) = crew.seats.get_mut("review-probe") {
            for s in staffings.iter_mut() {
                s.k = k;
            }
        }
    }

    let bundle_set = match &opts.bundler {
        Some(cmd) => external_bundles(cmd, opts.worktree.as_deref(), &opts.diff)?,
        None => build_bundles(&source, diff_text)?,
    };
    let bundles = bundle_inputs_from_set(&bundle_set, &source)?;

    let intent = match &opts.intent_file {
        Some(p) => std::fs::read_to_string(p)
            .with_context(|| format!("reading --intent-file {}", p.display()))?,
        None => String::new(),
    };

    let mode = parse_exec_mode(&opts.mode)?;
    let probe_system = darkmux_crew::loader::role_prompt("review-probe").ok_or_else(|| {
        anyhow!(
            "darkmux: role \"review-probe\" has no system prompt — reinstall darkmux or check \
             <crew_root>/roles/review-probe.md"
        )
    })?;
    let judge_system = darkmux_crew::loader::role_prompt("review-judge").ok_or_else(|| {
        anyhow!(
            "darkmux: role \"review-judge\" has no system prompt — reinstall darkmux or check \
             <crew_root>/roles/review-judge.md"
        )
    })?;
    // (#1260) The verify seat's persona — resolved unconditionally (the
    // role is embedded); only dispatched when the crew declares the seat.
    let verify_system = darkmux_crew::loader::role_prompt("review-verify").ok_or_else(|| {
        anyhow!(
            "darkmux: role \"review-verify\" has no system prompt — reinstall darkmux or check \
             <crew_root>/roles/review-verify.md"
        )
    })?;

    let case_id = match (&opts.github, &opts.head_sha) {
        (Some(repo), Some(sha)) => format!("{repo}@{sha}"),
        _ => opts
            .worktree
            .as_ref()
            .map(|w| w.display().to_string())
            .unwrap_or_else(|| "local".to_string()),
    };

    let inputs = FunnelInputs {
        case_id,
        crew: &crew,
        // No separate title on this CLI surface (`--intent-file` is one
        // blob) — the whole file becomes the body; `judge_prompt` renders
        // an empty title as a blank line, same as Phase A's own
        // title-absent case. See `RunOpts::intent_file`'s doc comment.
        intent_title: "",
        intent_body: &intent,
        diff: diff_text,
        mode,
        probe_system: &probe_system,
        judge_system: &judge_system,
        verify_system: &verify_system,
        bundles: Some(bundles),
        // (#1260) The per-execution remote token allowance, resolved through
        // the one precedence home (`env > config.remote.* > 500000`) — only
        // endpoint-staffed seats draw from it.
        remote_max_tokens_per_execution:
            darkmux_types::config_access::remote_max_tokens_per_execution(),
    };

    // (#1272) Bookend identity, captured before `inputs`/`crew` are moved
    // into the dispatch closures below — same case_id/handle `funnel.*`
    // records already carry (`inputs.case_id` / `crew.name`), so the
    // dispatch bookend and the funnel's own records group under identical
    // session_id/handle in the viewer.
    let case_id_for_bookends = inputs.case_id.clone();
    let crew_name_for_bookends = crew.name.clone();
    let model_for_bookends = crew_model_summary(&crew);

    let timeout = opts.timeout;
    // (#1260) Route on what the seat's profile declares (contract 1): an
    // endpoint-bearing call goes through the hosted dialect (Azure
    // api-version + Keychain auth + max_completion_tokens — the transport
    // `dispatch_remote` proved); a local call goes through LMStudio. The
    // system/user texts are identical either way (contract 6).
    let mut chat = move |call: &ChatCall| -> Result<SingleShotReply> {
        match call.endpoint {
            Some(endpoint) => single_shot_chat_hosted(&HostedSingleShotRequest {
                endpoint,
                model: call.model,
                system: call.system,
                user: call.user,
                max_tokens: call.max_tokens,
                timeout_seconds: timeout,
            }),
            None => single_shot_chat(&SingleShotRequest {
                base_url: None,
                model: call.model,
                system: call.system,
                user: call.user,
                temperature: call.temperature,
                max_tokens: call.max_tokens,
                timeout_seconds: timeout,
            }),
        }
    };
    let mut cycler = LmsCycler;
    let mut emitter = FleetFlowEmitter;

    if let Some(charges_path) = &opts.charges_file {
        let raw = std::fs::read_to_string(charges_path)
            .with_context(|| format!("reading --charges-file {}", charges_path.display()))?;
        let flags: Vec<ProbeFlag> = serde_json::from_str(&raw)
            .with_context(|| format!("parsing --charges-file {} as a flag list", charges_path.display()))?;
        with_dispatch_bookends(
            &mut emitter,
            &case_id_for_bookends,
            &crew_name_for_bookends,
            model_for_bookends.as_deref(),
            move |emitter| run_judge_only(flags, &inputs, &mut chat, &mut cycler, emitter),
        )
    } else {
        with_dispatch_bookends(
            &mut emitter,
            &case_id_for_bookends,
            &crew_name_for_bookends,
            model_for_bookends.as_deref(),
            move |emitter| run_funnel(&inputs, &mut chat, &mut cycler, emitter),
        )
    }
}

/// (#1222 Phase B packet 5) The fixed marker every `Tier::Confirmed`
/// finding carries. A local judge's double-confirm is real signal (the
/// CLAUDE.md "recheck vs rethink" doctrine's cross-context re-thinking, at
/// judge scale) — but it is not the frontier review the same doctrine
/// reserves for invariant/security-bearing diffs. The marker keeps that
/// distinction visible to whoever reads the posted comment.
const CONFIRMED_MARKER: &str =
    "⚠ needs frontier verification — confirmed by a local judge, not yet frontier-verified";

/// Three-tier synthesis of a [`FunnelEnvelope`] into the [`Rendered`]
/// `{mode, review, comment}` contract:
///
/// - [`Tier::Confirmed`] -> an inline review comment (anchor resolved via
///   [`new_side_index`]/[`resolve_anchor`] against `diff` — the same
///   discipline [`render_with_attribution`] uses) or, when the anchor can't
///   be resolved to exactly one diff line, a general body item — never a
///   guessed line. Any confirmed finding makes the review `REQUEST_CHANGES`
///   (merge-blocking).
/// - [`Tier::NeedsCheck`] (a pass-2 demotion already folds into this tier —
///   `judge_one_flag` never leaves a demoted flag `Confirmed`) -> one
///   non-blocking bullet in a "worth a double check" section: in the
///   review body when there's also a confirmed finding, or in the comment
///   when there isn't. (#1299) When the tier exceeded the clustering
///   threshold — `env.needs_check_clusters` is non-empty — the section
///   renders ONE "N related concerns in <file> around <mechanism>" bullet
///   per cluster instead of the raw per-finding bullets, so a duplicative
///   tier can't wall-of-text; the total count is conserved (the clusters'
///   counts sum to the raw needs_check total).
/// - [`Tier::Confirmed`] findings additionally surface any same-location
///   duplicate framings they ABSORBED at dedup (#1299 `also_flagged`) as a
///   trailing "Also flagged (same location): …" line — the "aggregate,
///   never discard" safety net, so a residual within-class merge can never
///   vanish a second defect's description.
/// - [`Tier::Archived`] -> never rendered; stays in the envelope only.
/// - Zero confirmed AND zero needs-check on a healthy (non-degenerate)
///   funnel -> an honest `"comment"` summary naming how much was
///   investigated plus which models ran it — never a silent green pass.
/// - A degenerate envelope (`env.degenerate.is_some()`) -> `"degraded"`,
///   the same contract [`degraded_fallback`] (#1113) uses elsewhere in
///   this module — no review signal was produced.
pub fn synthesize_funnel(env: &FunnelEnvelope, diff: &str, attribution: Option<&str>) -> Rendered {
    if let Some(note) = &env.degenerate {
        // (#1298) Even a degenerate run posts the envelope-derived footer, so a
        // remote crew that produced no signal never claims "no cloud API".
        return degraded_with_footer(
            &format!("The review funnel produced no signal: {note}."),
            &funnel_footer(env, attribution),
        );
    }

    let index = new_side_index(diff);
    let mut inline: Vec<Value> = Vec::new();
    let mut confirmed_general: Vec<String> = Vec::new();
    let mut needs_check_lines: Vec<String> = Vec::new();

    for j in &env.judged {
        let path = path_from_bundle_id(&j.flag.bundle_id);
        match j.tier {
            Tier::Confirmed => {
                let record = j.pass2.as_ref().unwrap_or(&j.pass1);
                // (#1260) A verify-seat `verified` ruling replaces the
                // manual-verification marker with the adjudication line;
                // `uncertain`/`unparsed`/`error` (or no verify seat at all)
                // keep the marker — an inconclusive adjudication never
                // promotes. `refuted` never reaches here (tier = Archived).
                let verified = j.verify.as_ref().filter(|v| v.ruling == VerifyRuling::Verified);
                match resolve_anchor(Some(path), j.flag.anchor.as_deref(), &index) {
                    Some(line) => inline.push(json!({
                        "path": norm_path(path),
                        "line": line,
                        "side": "RIGHT",
                        "body": confirmed_comment_body(record, verified, &j.flag.also_flagged),
                    })),
                    None => confirmed_general
                        .push(confirmed_general_bullet(path, record, verified, &j.flag.also_flagged)),
                }
            }
            Tier::NeedsCheck => {
                let record = j.pass2.as_ref().unwrap_or(&j.pass1);
                needs_check_lines.push(needs_check_bullet(path, j.flag.anchor.as_deref(), record));
            }
            Tier::Archived => {}
        }
    }

    // (#1299) The "worth a double check" section. When clustering fired
    // (needs_check exceeded the threshold — `env.needs_check_clusters`
    // non-empty), render one "N related concerns in <file> around
    // <mechanism>" bullet per cluster so a duplicative tier can't
    // wall-of-text. Below the threshold, render the raw per-finding bullets
    // as before. Either way the COUNT is conserved: `needs_check_count`
    // (the raw total) equals the sum of the clusters' counts, so nothing is
    // hidden — the wall just collapses to a handful of counted lines.
    let needs_check_count = needs_check_lines.len();
    let needs_check_section: Vec<String> = if env.needs_check_clusters.is_empty() {
        needs_check_lines
    } else {
        env.needs_check_clusters.iter().map(|c| format!("- {}", c.bullet())).collect()
    };

    let confirmed_total = inline.len() + confirmed_general.len();

    if confirmed_total == 0 {
        let mut lines = vec!["### 🤖 darkmux PR review".to_string(), String::new()];
        if needs_check_count == 0 {
            lines.push(format!(
                "review funnel ran: {} flags investigated across {} bundles, none confirmed. _{}_",
                env.deduped_flags,
                env.bundles,
                member_summary(env)
            ));
        } else {
            lines.push(format!(
                "review funnel ran: {} flags investigated across {} bundles, none confirmed — \
                 {} worth a double check (not merge-blocking). _{}_",
                env.deduped_flags,
                env.bundles,
                needs_check_count,
                member_summary(env)
            ));
            lines.push(String::new());
            lines.push("**Worth a double check:**".to_string());
            lines.extend(needs_check_section.iter().cloned());
        }
        lines.extend(run_warnings_block(env));
        return Rendered {
            mode: "comment",
            review: None,
            comment: Some(format!("{}{}", lines.join("\n"), funnel_footer(env, attribution))),
        };
    }

    let mut body = vec!["### 🤖 darkmux PR review".to_string(), String::new()];
    body.push(format!(
        "**Verdict: flag** · {} confirmed ({} inline, {} general)",
        confirmed_total,
        inline.len(),
        confirmed_general.len()
    ));
    if !confirmed_general.is_empty() {
        body.push(String::new());
        body.push("**Confirmed findings not anchored to a diff line:**".to_string());
        body.extend(confirmed_general);
    }
    if needs_check_count > 0 {
        body.push(String::new());
        body.push("**Worth a double check** (not merge-blocking):".to_string());
        body.extend(needs_check_section);
    }
    body.extend(run_warnings_block(env));

    Rendered {
        mode: "review",
        review: Some(json!({
            "event": "REQUEST_CHANGES",
            "body": format!("{}{}", body.join("\n"), funnel_footer(env, attribution)),
            "comments": inline,
        })),
        comment: None,
    }
}

/// (#1260) The loud run-warnings block appended to a posted review/comment —
/// non-fatal findings the operator must SEE on the PR, not just in the
/// envelope: a verify budget exhausting mid-stage (some confirmed findings
/// keep the manual-verification marker), or a remote probe seat failing
/// (reduced coverage). Empty (and byte-identical to pre-#1260 output) when
/// the run produced no warnings — a crew without remote seats never does.
fn run_warnings_block(env: &FunnelEnvelope) -> Vec<String> {
    if env.warnings.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![String::new(), "**⚠ Run warnings**".to_string()];
    lines.extend(env.warnings.iter().map(|w| format!("- {w}")));
    lines
}

/// (#1260) The frontier-verified line a `verified` adjudication earns —
/// replaces [`CONFIRMED_MARKER`] and names the adjudicating model, so the
/// posted comment says WHERE the verification came from (operator
/// sovereignty: the reader never wonders which tier signed off).
fn verified_line(v: &VerifyRecord) -> String {
    format!("✓ verified by {} adjudication", v.model)
}

fn confirmed_comment_body(
    record: &JudgeRecord,
    verified: Option<&VerifyRecord>,
    also_flagged: &[String],
) -> String {
    let mut lines = Vec::new();
    let note = record.note_for_author.trim();
    lines.push(if note.is_empty() { "(no note from the judge)".to_string() } else { note.to_string() });
    let evidence = record.decisive_evidence.trim();
    if !evidence.is_empty() {
        lines.push(format!("Evidence: {evidence}"));
    }
    match verified {
        Some(v) => lines.push(verified_line(v)),
        None => lines.push(CONFIRMED_MARKER.to_string()),
    }
    // (#1299) Surface the "aggregate, never discard" safety net: any
    // same-location duplicate framings this finding absorbed at dedup are
    // shown verbatim, so a residual within-class merge can never vanish a
    // second defect's description — the reviewer sees BOTH framings.
    if let Some(also) = also_flagged_line(also_flagged) {
        lines.push(also);
    }
    lines.join("\n\n")
}

fn confirmed_general_bullet(
    path: &str,
    record: &JudgeRecord,
    verified: Option<&VerifyRecord>,
    also_flagged: &[String],
) -> String {
    let note = record.note_for_author.trim();
    let mut line = format!(
        "- `{path}` — {}",
        if note.is_empty() { "(no note from the judge)" } else { note }
    );
    let evidence = record.decisive_evidence.trim();
    if !evidence.is_empty() {
        line.push_str(&format!(" _Evidence: {evidence}_"));
    }
    match verified {
        Some(v) => line.push_str(&format!(" ({})", verified_line(v))),
        None => line.push_str(&format!(" ({CONFIRMED_MARKER})")),
    }
    if let Some(also) = also_flagged_line(also_flagged) {
        line.push_str(&format!(" _{also}_"));
    }
    line
}

/// (#1299) The trailing "Also flagged (same location): …" line for a
/// confirmed finding that ABSORBED one or more same-location duplicate
/// framings during dedup. `None` when nothing was absorbed (the common
/// case), so an un-collapsed finding renders byte-identically to before.
fn also_flagged_line(also_flagged: &[String]) -> Option<String> {
    let framings: Vec<&str> =
        also_flagged.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if framings.is_empty() {
        return None;
    }
    Some(format!("Also flagged (same location): {}", framings.join("; ")))
}

fn needs_check_bullet(path: &str, anchor: Option<&str>, record: &JudgeRecord) -> String {
    let anchor_bit = anchor
        .and_then(|a| a.lines().find(|l| !l.trim().is_empty()))
        .map(|l| format!(" (`{}`)", l.trim()))
        .unwrap_or_default();
    let note = record.note_for_author.trim();
    format!(
        "- `{path}`{anchor_bit} — {}",
        if note.is_empty() { "(no note from the judge)" } else { note }
    )
}

/// "probed by <models>; judged by <model>" — the "member/model attribution"
/// half of the zero-confirms-healthy comment, distinct from the operator-
/// supplied `attribution` CLI flag (which governs the posted footer via
/// [`footer_for`]). Names WHICH local models ran the funnel, not WHERE it
/// ran.
fn member_summary(env: &FunnelEnvelope) -> String {
    let probes: Vec<&str> = env
        .members
        .iter()
        .filter(|m| m.seat == "review-probe")
        .map(|m| m.model.as_str())
        .collect();
    let judge = env
        .members
        .iter()
        .find(|m| m.seat == "review-judge")
        .map(|m| m.model.as_str())
        .unwrap_or("unknown");
    format!(
        "probed by {}; judged by {judge}",
        if probes.is_empty() { "unknown".to_string() } else { probes.join(", ") }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    // Test-only: these funnel types aren't referenced by this module's
    // production code (only inferred through `env.judged`/`env.members`
    // iteration), so importing them at file scope would warn "unused" on a
    // plain (non-test) `cargo build`.
    use darkmux_lab::lab::funnel::{FunnelRuling, JudgedFlag, MemberRecord, NeedsCheckCluster};

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
        assert_eq!(render(&env("{\"summary\":\"s\",\"verdict\":\"pass\",\"findings\":[]}"), DIFF).mode, "review");
    }

    #[test]
    fn render_fence_without_json_tag() {
        assert_eq!(render(&env("```\n{\"summary\":\"s\",\"verdict\":\"pass\",\"findings\":[]}\n```"), DIFF).mode, "review");
    }

    #[test]
    fn render_prose_around_json() {
        let r = render(&env("Here is my review:\n{\"summary\":\"s\",\"verdict\":\"pass\",\"findings\":[]}\nThanks!"), DIFF);
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
        let rv = render(&env("{\"summary\":\"s\",\"verdict\":\"pass\",\"findings\":[]}"), DIFF).review.unwrap();
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
        let body = render(&env("{\"summary\":\"s\",\"findings\":[]}"), DIFF).review.unwrap()["body"].as_str().unwrap().to_string();
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
        // (#1113) A missing envelope is NO review signal — degraded, not a
        // comment a workflow could mistake for benign.
        assert_eq!(v["mode"], "degraded");
        assert!(v["comment"].as_str().unwrap().contains("no envelope"));
    }

    #[test]
    fn cmd_render_emits_review_payload_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.json");
        let envf = dir.path().join("env.json");
        let difff = dir.path().join("d.diff");
        std::fs::write(&envf, env("{\"summary\":\"s\",\"verdict\":\"pass\",\"findings\":[]}")).unwrap();
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
        let r = render(&env("{\"summary\":\"s\",\"verdict\":\"pass\",\"findings\":null}"), DIFF);
        assert_eq!(r.mode, "review");
        let rv = r.review.unwrap();
        assert_eq!(rv["comments"].as_array().unwrap().len(), 0);
        assert!(rv["body"].as_str().unwrap().contains("0 inline, 0 general"));
    }

    #[test]
    fn render_empty_reply_is_degraded_not_a_pass() {
        // (#1113) An empty reply is NO review signal — it must not be
        // indistinguishable from a clean review.
        let r = render(&env(""), DIFF);
        assert_eq!(r.mode, "degraded");
        let c = r.comment.unwrap();
        assert!(c.contains("no review signal"), "{c}");
        assert!(c.contains("not a clean pass"), "{c}");
    }

    // ─── #1113: vacuous-pass gate ──────────────────────────────────────

    #[test]
    fn vacuous_structured_pass_degrades() {
        // pass + zero findings + no summary proves nothing was reviewed.
        for reply in [
            "{\"verdict\":\"pass\",\"findings\":[]}",
            "{\"findings\":[]}",
            "{\"summary\":\"  \",\"verdict\":\"pass\",\"findings\":[]}",
            // Off-contract pass synonyms must not slip through the gate.
            "{\"verdict\":\"passed\",\"findings\":[]}",
            "{\"verdict\":\"LGTM\",\"findings\":[]}",
        ] {
            let r = render(&env(reply), DIFF);
            assert_eq!(r.mode, "degraded", "must degrade: {reply}");
            assert!(r.comment.unwrap().contains("empty pass"));
        }
    }

    #[test]
    fn clean_pass_with_summary_is_a_real_review() {
        let r = render(
            &env("{\"summary\":\"Change achieves its purpose.\",\"verdict\":\"pass\",\"findings\":[]}"),
            DIFF,
        );
        assert_eq!(r.mode, "review", "a summarized clean pass is real signal");
    }

    #[test]
    fn flag_with_findings_but_no_summary_never_degrades() {
        // Real findings are real signal regardless of the missing summary.
        let reply = "{\"verdict\":\"flag\",\"findings\":[{\"path\":\"src/x.ts\",\"anchor\":\"const b = 2;\",\"severity\":\"high\",\"title\":\"t\",\"detail\":\"d\",\"advice\":\"a\",\"suggestion\":null}]}";
        let r = render(&env(reply), DIFF);
        assert_eq!(r.mode, "review");
        assert_eq!(r.review.unwrap()["comments"].as_array().unwrap().len(), 1);
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
            &env("{\"summary\":\"s\",\"verdict\":\"pass\",\"findings\":[]}"),
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
        let d = render(&env("{\"summary\":\"s\",\"verdict\":\"pass\",\"findings\":[]}"), DIFF);
        assert!(d.review.unwrap()["body"]
            .as_str()
            .unwrap()
            .contains("no cloud API"));
    }

    // ─── synthesize_funnel: three-tier synthesis (#1222 Phase B packet 5) ──

    fn judge_record(ruling: FunnelRuling, evidence: &str, note: &str) -> JudgeRecord {
        JudgeRecord {
            ruling,
            decisive_evidence: evidence.to_string(),
            note_for_author: note.to_string(),
            pass: 1,
            seconds: 0.1,
        }
    }

    fn probe_flag(bundle_id: &str, anchor: Option<&str>) -> ProbeFlag {
        ProbeFlag {
            bundle_id: bundle_id.to_string(),
            fact_family: "unscoped".to_string(),
            member: "darkmux:probe-model".to_string(),
            draw: 0,
            charge_text: "a flagged concern".to_string(),
            anchor: anchor.map(str::to_string),
            also_flagged: Vec::new(),
        }
    }

    /// A double-confirmed flag: pass-1 AND pass-2 both `confirmed` — the
    /// only way `judge_one_flag` (funnel.rs) ever produces `Tier::Confirmed`.
    fn confirmed_flag(bundle_id: &str, anchor: Option<&str>, note: &str, evidence: &str) -> JudgedFlag {
        let record = judge_record(FunnelRuling::Confirmed, evidence, note);
        JudgedFlag {
            flag: probe_flag(bundle_id, anchor),
            pass1: record.clone(),
            pass2: Some(record),
            tier: Tier::Confirmed,
            demoted_by_pass2: false,
                verify: None,
                demoted_by_verify: false,
        }
    }

    /// `demoted = true` mirrors a pass-1 `confirmed` that pass-2 disagreed
    /// with (`demoted_by_pass2 = true`, per `judge_one_flag`'s state
    /// machine); `demoted = false` is a plain pass-1 `needs_check` with no
    /// pass-2 at all. Either way `tier` is `NeedsCheck` — `synthesize_funnel`
    /// doesn't special-case `demoted_by_pass2` itself, it just reads `tier`.
    fn needs_check_flag(bundle_id: &str, anchor: Option<&str>, note: &str, demoted: bool) -> JudgedFlag {
        if demoted {
            JudgedFlag {
                flag: probe_flag(bundle_id, anchor),
                pass1: judge_record(FunnelRuling::Confirmed, "pass-1 evidence", "pass-1 note"),
                pass2: Some(judge_record(FunnelRuling::FalsePositive, "pass-2 evidence", note)),
                tier: Tier::NeedsCheck,
                demoted_by_pass2: true,
                    verify: None,
                    demoted_by_verify: false,
            }
        } else {
            JudgedFlag {
                flag: probe_flag(bundle_id, anchor),
                pass1: judge_record(FunnelRuling::NeedsCheck, "pass-1 evidence", note),
                pass2: None,
                tier: Tier::NeedsCheck,
                demoted_by_pass2: false,
                    verify: None,
                    demoted_by_verify: false,
            }
        }
    }

    fn archived_flag(bundle_id: &str) -> JudgedFlag {
        JudgedFlag {
            flag: probe_flag(bundle_id, None),
            pass1: judge_record(FunnelRuling::FalsePositive, "not a real issue", "no action needed"),
            pass2: None,
            tier: Tier::Archived,
            demoted_by_pass2: false,
                verify: None,
                demoted_by_verify: false,
        }
    }

    fn healthy_envelope(judged: Vec<JudgedFlag>) -> FunnelEnvelope {
        let distinct_bundles: std::collections::HashSet<&str> =
            judged.iter().map(|j| j.flag.bundle_id.as_str()).collect();
        FunnelEnvelope {
            deduped_flags: judged.len(),
            bundles: distinct_bundles.len().max(1),
            judged,
            members: vec![
                MemberRecord { model: "darkmux:probe-model".into(), seat: "review-probe".into(), draws: 2, ..Default::default() },
                MemberRecord { model: "darkmux:judge-model".into(), seat: "review-judge".into(), draws: 2, ..Default::default() },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn synthesize_confirmed_resolves_inline_with_marker_and_request_changes() {
        let j = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "shadows the config default",
            "the clamp is bypassed",
        );
        let env = healthy_envelope(vec![j]);
        let r = synthesize_funnel(&env, DIFF, None);
        assert_eq!(r.mode, "review");
        let review = r.review.unwrap();
        assert_eq!(review["event"], "REQUEST_CHANGES", "a confirmed finding merge-blocks");
        let comments = review["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["path"], "src/x.ts", "bundle_id's fn@path is split on the last @");
        assert_eq!(comments[0]["line"], 2);
        let body = comments[0]["body"].as_str().unwrap();
        assert!(body.contains("shadows the config default"), "{body}");
        assert!(body.contains("the clamp is bypassed"), "{body}");
        assert!(body.contains(CONFIRMED_MARKER), "{body}");
        // note_for_author must precede decisive_evidence must precede the marker.
        let note_at = body.find("shadows the config default").unwrap();
        let evidence_at = body.find("the clamp is bypassed").unwrap();
        let marker_at = body.find(CONFIRMED_MARKER).unwrap();
        assert!(note_at < evidence_at && evidence_at < marker_at, "{body}");
    }

    #[test]
    fn synthesize_confirmed_unresolvable_anchor_lands_in_body_not_inline() {
        let j = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("this text never appears in the diff"),
            "a note the judge left",
            "some evidence",
        );
        let env = healthy_envelope(vec![j]);
        let r = synthesize_funnel(&env, DIFF, None);
        assert_eq!(r.mode, "review");
        let review = r.review.unwrap();
        assert_eq!(
            review["comments"].as_array().unwrap().len(),
            0,
            "an unresolvable anchor must never guess a line"
        );
        let body = review["body"].as_str().unwrap();
        assert!(body.contains("not anchored to a diff line"), "{body}");
        assert!(body.contains("a note the judge left"), "{body}");
        assert!(body.contains(CONFIRMED_MARKER), "{body}");
    }

    #[test]
    fn synthesize_confirmed_file_level_anchor_also_lands_in_body() {
        // `anchor: None` (a file-level charge) is equally unresolvable —
        // `resolve_anchor` returns `None` for a `None` anchor.
        let j = confirmed_flag("computeEnd@src/x.ts", None, "file-level concern", "evidence");
        let env = healthy_envelope(vec![j]);
        let r = synthesize_funnel(&env, DIFF, None);
        let review = r.review.unwrap();
        assert_eq!(review["comments"].as_array().unwrap().len(), 0);
        assert!(review["body"].as_str().unwrap().contains("file-level concern"));
    }

    #[test]
    fn synthesize_demoted_flag_lands_in_needs_check_not_confirmed() {
        let confirmed =
            confirmed_flag("computeEnd@src/x.ts", Some("const b = 2;"), "real bug", "evidence");
        let demoted = needs_check_flag(
            "otherFn@src/y.ts",
            None,
            "the judge flip-flopped on this one",
            true,
        );
        let env = healthy_envelope(vec![confirmed, demoted]);
        let r = synthesize_funnel(&env, DIFF, None);
        assert_eq!(r.mode, "review");
        let body = r.review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(body.contains("1 confirmed (1 inline, 0 general)"), "{body}");
        assert!(body.contains("Worth a double check"), "{body}");
        assert!(body.contains("the judge flip-flopped on this one"), "{body}");
    }

    #[test]
    fn synthesize_plain_needs_check_without_demotion_also_lands_in_section() {
        let nc = needs_check_flag(
            "computeEnd@src/x.ts",
            Some("const c = 3;"),
            "worth a second look",
            false,
        );
        let confirmed =
            confirmed_flag("otherFn@src/y.ts", Some("const b = 2;"), "real bug", "evidence");
        let env = healthy_envelope(vec![confirmed, nc]);
        let r = synthesize_funnel(&env, DIFF, None);
        let body = r.review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(body.contains("worth a second look"), "{body}");
        assert!(body.contains("const c = 3;"), "anchor is named in the bullet: {body}");
    }

    #[test]
    fn synthesize_zero_confirms_with_needs_check_stays_comment_mode() {
        // "the comment when there are zero confirms" — needs_check items
        // never open a REQUEST_CHANGES review on their own.
        let nc = needs_check_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "double check this one",
            false,
        );
        let env = healthy_envelope(vec![nc]);
        let r = synthesize_funnel(&env, DIFF, None);
        assert_eq!(r.mode, "comment", "zero confirms never opens a review");
        let c = r.comment.unwrap();
        assert!(c.contains("worth a double check"), "{c}");
        assert!(c.contains("double check this one"), "{c}");
    }

    #[test]
    fn synthesize_zero_confirms_zero_needs_check_healthy_is_honest_comment() {
        let env = FunnelEnvelope {
            deduped_flags: 3,
            bundles: 2,
            judged: vec![archived_flag("computeEnd@src/x.ts"), archived_flag("otherFn@src/y.ts")],
            members: vec![
                MemberRecord { model: "darkmux:probe-a".into(), seat: "review-probe".into(), ..Default::default() },
                MemberRecord { model: "darkmux:judge-b".into(), seat: "review-judge".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        let r = synthesize_funnel(&env, DIFF, None);
        assert_eq!(r.mode, "comment");
        let c = r.comment.unwrap();
        assert!(
            c.contains("review funnel ran: 3 flags investigated across 2 bundles, none confirmed"),
            "{c}"
        );
        assert!(c.contains("darkmux:probe-a"), "member attribution: {c}");
        assert!(c.contains("darkmux:judge-b"), "member attribution: {c}");
    }

    #[test]
    fn synthesize_archived_never_appears_in_rendered_output() {
        let confirmed = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "a note that should survive",
            "evidence",
        );
        let archived = archived_flag("suspicious-archived-bundle-id@src/y.ts");
        let env = healthy_envelope(vec![confirmed, archived]);
        let r = synthesize_funnel(&env, DIFF, None);
        let review = r.review.unwrap();
        let body = review["body"].as_str().unwrap().to_string();
        assert!(
            !body.contains("suspicious-archived-bundle-id"),
            "an archived flag must never render: {body}"
        );
        // The confirmed flag's anchor resolves, so its note lands in the
        // inline comment body, not the review's summary body.
        let comments = review["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1);
        assert!(comments[0]["body"].as_str().unwrap().contains("a note that should survive"));
    }

    #[test]
    fn synthesize_degenerate_envelope_is_degraded() {
        let env = FunnelEnvelope {
            degenerate: Some("zero flags from all probe draws — never a silent pass".to_string()),
            ..Default::default()
        };
        let r = synthesize_funnel(&env, DIFF, None);
        assert_eq!(r.mode, "degraded");
        let c = r.comment.unwrap();
        assert!(c.contains("no signal"), "{c}");
        assert!(c.contains("zero flags from all probe draws"), "{c}");
    }

    #[test]
    fn path_from_bundle_id_splits_on_first_at_preserving_scoped_paths() {
        // <fn>@<path> — fn names never contain '@', paths can (npm @scope).
        assert_eq!(path_from_bundle_id("computeEnd@src/x.ts"), "src/x.ts");
        assert_eq!(
            path_from_bundle_id("helper@vendor/@scope/pkg/index.ts"),
            "vendor/@scope/pkg/index.ts"
        );
        assert_eq!(path_from_bundle_id("plain-path.ts"), "plain-path.ts"); // no '@'
    }

    #[test]
    fn synthesize_judge_dead_envelope_is_degraded_not_honest_comment() {
        // The judge-dead honesty gate's synthesis half (#1222 packet 5
        // review): the envelope finish_funnel now produces when every judge
        // ruling was Unparsed/Error — judged flags all Archived, zero
        // confirms/needs-check, degenerate SET — must route to "degraded",
        // never to the honest "N flags investigated, none confirmed"
        // comment.
        let mut env = healthy_envelope(vec![JudgedFlag {
            flag: probe_flag("computeEnd@src/x.ts", None),
            pass1: judge_record(FunnelRuling::Unparsed, "", ""),
            pass2: None,
            tier: Tier::Archived,
            demoted_by_pass2: false,
                verify: None,
                demoted_by_verify: false,
        }]);
        env.degenerate =
            Some("judge produced no usable ruling on any of 1 flags (all errored/unparsed)".to_string());
        let r = synthesize_funnel(&env, DIFF, None);
        assert_eq!(r.mode, "degraded", "a dead judge must never render green");
        let c = r.comment.unwrap();
        assert!(c.contains("no usable ruling"), "{c}");
        assert!(!c.contains("none confirmed"), "must not read like an honest pass: {c}");
    }

    #[test]
    fn synthesize_attribution_flows_into_footer() {
        let confirmed =
            confirmed_flag("computeEnd@src/x.ts", Some("const b = 2;"), "n", "e");
        let att = "Reviewed by the review funnel on the repo's self-hosted runner.";
        let env = healthy_envelope(vec![confirmed]);
        let r = synthesize_funnel(&env, DIFF, Some(att));
        let body = r.review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(body.contains(att), "{body}");
    }

    // ─── #1298: footer dispatch-provenance DERIVED from the envelope ──────

    fn probe_member(model: &str, remote: bool) -> MemberRecord {
        MemberRecord { model: model.into(), seat: "review-probe".into(), remote, ..Default::default() }
    }
    fn judge_member(model: &str, remote: bool) -> MemberRecord {
        MemberRecord { model: model.into(), seat: "review-judge".into(), remote, ..Default::default() }
    }

    /// A review-mode envelope (one confirmed flag ⇒ a footer-bearing `body`)
    /// staffed by exactly `members`, then its rendered review body.
    fn footer_body_for(members: Vec<MemberRecord>) -> String {
        let confirmed = confirmed_flag("computeEnd@src/x.ts", Some("const b = 2;"), "n", "e");
        let env = FunnelEnvelope { members, ..healthy_envelope(vec![confirmed]) };
        synthesize_funnel(&env, DIFF, None).review.unwrap()["body"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn footer_all_remote_says_hosted_names_model_never_no_cloud_api() {
        let body = footer_body_for(vec![probe_member("gpt-4o", true), judge_member("gpt-4o", true)]);
        assert!(body.contains("hosted cloud endpoint"), "{body}");
        assert!(body.contains("gpt-4o"), "names the remote model: {body}");
        assert!(!body.contains("no cloud API"), "never claims no cloud API on a remote run: {body}");
    }

    #[test]
    fn footer_all_local_keeps_local_model_no_cloud_api() {
        let body = footer_body_for(vec![
            probe_member("darkmux:qwen-probe", false),
            judge_member("darkmux:qwen-judge", false),
        ]);
        assert!(body.contains("local model, no cloud API"), "{body}");
        assert!(body.contains("darkmux:qwen-judge"), "names a local seat model: {body}");
    }

    #[test]
    fn footer_mixed_crew_names_both_and_never_no_cloud_api() {
        let body = footer_body_for(vec![
            probe_member("darkmux:qwen-probe", false),
            judge_member("gpt-4o", true),
        ]);
        assert!(body.contains("darkmux:qwen-probe"), "names the local seat: {body}");
        assert!(body.contains("gpt-4o"), "names the hosted seat: {body}");
        assert!(!body.contains("no cloud API"), "a mixed crew must not claim no cloud API: {body}");
    }

    #[test]
    fn footer_drops_stale_pr_reviewer_role_wording() {
        let body = footer_body_for(vec![
            probe_member("darkmux:qwen-probe", false),
            judge_member("darkmux:qwen-judge", false),
        ]);
        assert!(!body.contains("pr-reviewer"), "the stale role name must be gone: {body}");
        assert!(body.contains("review funnel"), "names the crew/funnel instead: {body}");
    }

    // ─── synthesize_funnel: coverage sweep (#1222 Phase B packet 5 QA) ────

    #[test]
    fn synthesize_confirmed_ambiguous_anchor_falls_to_general() {
        // The `hits.len() == 1` discipline (`resolve_anchor`) must hold at
        // the synthesize_funnel level too: a confirmed flag whose anchor
        // matches TWO diff lines must never guess — it lands in the general
        // body list, same as an unresolvable anchor. Mirrors
        // `resolve_ambiguous_duplicate_is_general` (render()-level) one
        // layer up.
        let dup_diff = "diff --git a/y.ts b/y.ts\n+++ b/y.ts\n@@ -1,0 +1,2 @@\n+  return;\n+  return;\n";
        let j = confirmed_flag("fn@y.ts", Some("return;"), "ambiguous note", "ambiguous evidence");
        let env = healthy_envelope(vec![j]);
        let r = synthesize_funnel(&env, dup_diff, None);
        assert_eq!(r.mode, "review");
        let review = r.review.unwrap();
        assert_eq!(
            review["comments"].as_array().unwrap().len(),
            0,
            "an ambiguous (multi-hit) anchor must never guess a line"
        );
        let body = review["body"].as_str().unwrap();
        assert!(body.contains("not anchored to a diff line"), "{body}");
        assert!(body.contains("ambiguous note"), "{body}");
    }

    #[test]
    fn confirmed_comment_body_empty_note_and_evidence_uses_fallback_text() {
        let record = judge_record(FunnelRuling::Confirmed, "", "");
        let body = confirmed_comment_body(&record, None, &[]);
        assert!(body.contains("(no note from the judge)"), "{body}");
        assert!(!body.contains("Evidence:"), "empty evidence must not render a line: {body}");
        assert!(body.contains(CONFIRMED_MARKER), "{body}");
    }

    // ─── the verify seat's tier mechanics (#1260) ─────────────────────

    fn verify_record(ruling: VerifyRuling) -> VerifyRecord {
        VerifyRecord {
            ruling,
            decisive_evidence: "the adjudicated line".to_string(),
            note_for_author: "adjudication note".to_string(),
            seconds: 1.0,
            model: "gpt-5.1".to_string(),
        }
    }

    /// (#1260) A `verified` adjudication posts as frontier-verified: the
    /// "⚠ needs frontier verification" marker is REPLACED by the
    /// "verified by <model> adjudication" line — inline and general alike.
    #[test]
    fn synthesize_verified_finding_drops_marker_and_names_adjudicator() {
        // Inline (anchor resolves).
        let mut j = confirmed_flag("computeEnd@src/x.ts", Some("const b = 2;"), "note", "evidence");
        j.verify = Some(verify_record(VerifyRuling::Verified));
        let env = healthy_envelope(vec![j]);
        let r = synthesize_funnel(&env, DIFF, None);
        assert_eq!(r.mode, "review", "a verified finding is still merge-blocking");
        let review = r.review.unwrap();
        let body = review["comments"][0]["body"].as_str().unwrap();
        assert!(body.contains("verified by gpt-5.1 adjudication"), "{body}");
        assert!(!body.contains(CONFIRMED_MARKER), "the manual-verification marker must be gone: {body}");

        // General (anchor unresolvable) — same replacement.
        let mut g = confirmed_flag("ghost@src/nowhere.ts", Some("no such line"), "note", "evidence");
        g.verify = Some(verify_record(VerifyRuling::Verified));
        let env = healthy_envelope(vec![g]);
        let r = synthesize_funnel(&env, DIFF, None);
        let body = r.review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(body.contains("verified by gpt-5.1 adjudication"), "{body}");
        assert!(!body.contains(CONFIRMED_MARKER), "{body}");
    }

    /// (#1260) An `uncertain` adjudication keeps the EXISTING marker —
    /// inconclusive never promotes; the posted bytes match a no-seat crew.
    #[test]
    fn synthesize_uncertain_verify_keeps_the_marker() {
        let mut j = confirmed_flag("computeEnd@src/x.ts", Some("const b = 2;"), "note", "evidence");
        j.verify = Some(verify_record(VerifyRuling::Uncertain));
        let env = healthy_envelope(vec![j]);
        let with_uncertain = synthesize_funnel(&env, DIFF, None);

        let no_seat = confirmed_flag("computeEnd@src/x.ts", Some("const b = 2;"), "note", "evidence");
        let env2 = healthy_envelope(vec![no_seat]);
        let without_seat = synthesize_funnel(&env2, DIFF, None);

        let a = with_uncertain.review.unwrap();
        let b = without_seat.review.unwrap();
        assert_eq!(a, b, "an uncertain adjudication renders byte-identically to no seat at all");
        assert!(a["comments"][0]["body"].as_str().unwrap().contains(CONFIRMED_MARKER));
    }

    /// (FIX 3 / #1260, ruling applied) A verify budget that exhausts
    /// MID-STAGE degrades the STAGE, not the run: verified findings still post
    /// as frontier-verified, each skipped adjudication (recorded per-flag as
    /// `VerifyRuling::Error`, tier still Confirmed) keeps the
    /// manual-verification marker, and the posted review carries the loud
    /// "verify budget exhausted after N of M adjudications" warning. The
    /// envelope is NOT degenerate — never routed to "produced no signal".
    #[test]
    fn synthesize_verify_exhaustion_posts_verified_plus_markered_plus_warning() {
        let mut verified = confirmed_flag("a@src/x.ts", Some("const b = 2;"), "verified note", "e");
        verified.verify = Some(verify_record(VerifyRuling::Verified));
        // A skipped adjudication: recorded per-flag as Error, stays Confirmed
        // — synthesize must keep its manual-verification marker.
        let mut skipped = confirmed_flag("b@src/x.ts", Some("const d = 5;"), "skipped note", "e");
        skipped.verify = Some(VerifyRecord {
            ruling: VerifyRuling::Error,
            decisive_evidence: String::new(),
            note_for_author: "remote token budget exhausted for this stage — call skipped".to_string(),
            seconds: 0.0,
            model: "gpt-5.1".to_string(),
        });
        let mut env = healthy_envelope(vec![verified, skipped]);
        env.warnings = vec![
            "verify budget exhausted after 1 of 2 adjudications — the remaining 1 confirmed \
             finding(s) keep the manual-verification marker (the per-execution allowance of 100 \
             tokens ran out)"
                .to_string(),
        ];

        let r = synthesize_funnel(&env, DIFF, None);
        assert_eq!(r.mode, "review", "confirmed findings still merge-block; never degraded");
        let review = r.review.unwrap();
        let comments = serde_json::to_string(&review["comments"]).unwrap();
        assert!(comments.contains("verified by gpt-5.1 adjudication"), "the verified one posts verified: {comments}");
        assert!(comments.contains(CONFIRMED_MARKER), "the skipped adjudication keeps the marker: {comments}");
        let body = review["body"].as_str().unwrap();
        assert!(body.contains("Run warnings"), "the warnings block renders on the review: {body}");
        assert!(body.contains("verify budget exhausted after 1 of 2 adjudications"), "{body}");
    }

    /// (#1260) A REFUTED finding arrives already demoted (tier = Archived,
    /// `demoted_by_verify` recorded in the envelope) — it never renders.
    #[test]
    fn synthesize_refuted_finding_never_renders() {
        let mut refuted = confirmed_flag("computeEnd@src/x.ts", Some("const b = 2;"), "refuted note", "e");
        refuted.tier = Tier::Archived;
        refuted.demoted_by_verify = true;
        refuted.verify = Some(verify_record(VerifyRuling::Refuted));
        let kept = confirmed_flag("other@src/x.ts", Some("const d = 5;"), "kept note", "e");
        let env = healthy_envelope(vec![refuted, kept]);
        let r = synthesize_funnel(&env, DIFF, None);
        let review = r.review.unwrap();
        let body = review["body"].as_str().unwrap();
        let comments = serde_json::to_string(&review["comments"]).unwrap();
        assert!(!body.contains("refuted note") && !comments.contains("refuted note"));
        assert!(comments.contains("kept note"), "the surviving confirm still posts");
    }

    #[test]
    fn confirmed_general_bullet_empty_note_and_evidence_uses_fallback_text() {
        let record = judge_record(FunnelRuling::Confirmed, "", "");
        let line = confirmed_general_bullet("src/x.ts", &record, None, &[]);
        assert!(line.contains("(no note from the judge)"), "{line}");
        assert!(!line.contains("_Evidence:"), "empty evidence must not render a line: {line}");
        assert!(line.contains(CONFIRMED_MARKER), "{line}");
    }

    #[test]
    fn needs_check_bullet_empty_note_uses_fallback_text() {
        let record = judge_record(FunnelRuling::NeedsCheck, "some evidence", "");
        let line = needs_check_bullet("src/x.ts", Some("const b = 2;"), &record);
        assert!(line.contains("(no note from the judge)"), "{line}");
        assert!(line.contains("const b = 2;"), "the anchor is still named: {line}");
    }

    #[test]
    fn synthesize_confirmed_empty_note_and_evidence_renders_fallback_inline() {
        // End-to-end companion to the two direct-fn tests above: an
        // anchor-resolved confirmed finding with empty judge fields must
        // still render, with the fallback text, not blank lines.
        let j = confirmed_flag("computeEnd@src/x.ts", Some("const b = 2;"), "", "");
        let env = healthy_envelope(vec![j]);
        let r = synthesize_funnel(&env, DIFF, None);
        let review = r.review.unwrap();
        let comments = review["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1);
        let body = comments[0]["body"].as_str().unwrap();
        assert!(body.contains("(no note from the judge)"), "{body}");
        assert!(!body.contains("Evidence:"), "{body}");
        assert!(body.contains(CONFIRMED_MARKER), "{body}");
    }

    #[test]
    fn synthesize_npm_scoped_path_end_to_end_resolves_inline() {
        // (#1222 packet 5 review) the bundle_id `<fn>@<path>` split-on-
        // first-`@` fix must hold end-to-end through synthesize_funnel — a
        // path that itself contains `@` (an npm `@scope/pkg` vendored
        // path) must resolve to an inline comment on the RIGHT path, not
        // just at `path_from_bundle_id`'s own unit level.
        let scoped_diff = "diff --git a/vendor/@scope/pkg/index.ts b/vendor/@scope/pkg/index.ts\n+++ b/vendor/@scope/pkg/index.ts\n@@ -1,1 +1,2 @@\n const existing = 1;\n+const scoped = 2;\n";
        let j = confirmed_flag(
            "helper@vendor/@scope/pkg/index.ts",
            Some("const scoped = 2;"),
            "scoped-path note",
            "scoped-path evidence",
        );
        let env = healthy_envelope(vec![j]);
        let r = synthesize_funnel(&env, scoped_diff, None);
        assert_eq!(r.mode, "review");
        let review = r.review.unwrap();
        let comments = review["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1, "the scoped path must resolve inline, not defer to general");
        assert_eq!(comments[0]["path"], "vendor/@scope/pkg/index.ts");
        assert_eq!(comments[0]["line"], 2);
    }

    #[test]
    fn synthesize_only_needs_check_never_opens_a_review_object() {
        // Explicit companion to
        // `synthesize_zero_confirms_with_needs_check_stays_comment_mode`:
        // confirms `review` is truly `None` (not merely that `mode ==
        // "comment"`), so a REQUEST_CHANGES event can never leak through
        // this path even indirectly.
        let nc = needs_check_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "double check this",
            false,
        );
        let env = healthy_envelope(vec![nc]);
        let r = synthesize_funnel(&env, DIFF, None);
        assert!(r.review.is_none(), "needs-check-only must never populate `review`");
        assert!(r.comment.is_some());
    }

    #[test]
    fn synthesize_zero_confirms_with_needs_check_also_names_members() {
        // The "N flags investigated ... worth a double check" branch calls
        // `member_summary` too (not only the pure-honest-zero branch) — its
        // attribution must name the actual models, not be silently
        // dropped.
        let nc = needs_check_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "double check",
            false,
        );
        let env = healthy_envelope(vec![nc]); // darkmux:probe-model / darkmux:judge-model
        let r = synthesize_funnel(&env, DIFF, None);
        let c = r.comment.unwrap();
        assert!(c.contains("darkmux:probe-model"), "{c}");
        assert!(c.contains("darkmux:judge-model"), "{c}");
    }

    #[test]
    fn synthesize_honest_zero_comment_names_multiple_probe_models() {
        let env = FunnelEnvelope {
            deduped_flags: 4,
            bundles: 2,
            judged: vec![archived_flag("a@f1.ts"), archived_flag("b@f2.ts")],
            members: vec![
                MemberRecord { model: "darkmux:probe-a".into(), seat: "review-probe".into(), ..Default::default() },
                MemberRecord { model: "darkmux:probe-b".into(), seat: "review-probe".into(), ..Default::default() },
                MemberRecord { model: "darkmux:judge-c".into(), seat: "review-judge".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        let r = synthesize_funnel(&env, DIFF, None);
        let c = r.comment.unwrap();
        assert!(c.contains("probed by darkmux:probe-a, darkmux:probe-b"), "{c}");
        assert!(c.contains("judged by darkmux:judge-c"), "{c}");
    }

    #[test]
    fn synthesize_honest_zero_comment_falls_back_to_unknown_with_no_members() {
        let env = FunnelEnvelope {
            deduped_flags: 1,
            bundles: 1,
            judged: vec![archived_flag("a@f1.ts")],
            members: vec![],
            ..Default::default()
        };
        let r = synthesize_funnel(&env, DIFF, None);
        let c = r.comment.unwrap();
        assert!(c.contains("probed by unknown"), "{c}");
        assert!(c.contains("judged by unknown"), "{c}");
    }

    // ── #1299: needs_check clustering + also_flagged rendering ────────────

    /// When clustering fired (`needs_check_clusters` non-empty), the "worth a
    /// double check" section renders ONE bullet per cluster — the wall of raw
    /// per-finding bullets collapses — while the total count is conserved.
    #[test]
    fn synthesize_needs_check_renders_clusters_when_clustering_fired() {
        let mut judged = Vec::new();
        for _ in 0..6 {
            judged.push(needs_check_flag("svcA@src/a.ts", None, "a bounds concern", false));
        }
        for _ in 0..4 {
            judged.push(needs_check_flag("svcB@src/b.ts", None, "a provenance concern", false));
        }
        let env = FunnelEnvelope {
            deduped_flags: judged.len(),
            bundles: 2,
            judged,
            needs_check_clusters: vec![
                NeedsCheckCluster { file: "svcA@src/a.ts".into(), mechanism: "null/bounds".into(), count: 6 },
                NeedsCheckCluster {
                    file: "svcB@src/b.ts".into(),
                    mechanism: "provenance/sibling".into(),
                    count: 4,
                },
            ],
            members: vec![
                MemberRecord { model: "darkmux:probe-model".into(), seat: "review-probe".into(), draws: 2, ..Default::default() },
                MemberRecord { model: "darkmux:judge-model".into(), seat: "review-judge".into(), draws: 2, ..Default::default() },
            ],
            ..Default::default()
        };
        let r = synthesize_funnel(&env, DIFF, None);
        assert_eq!(r.mode, "comment", "zero confirmed → comment mode");
        let c = r.comment.unwrap();
        // The raw total is conserved in the summary (10, not the 2 clusters).
        assert!(c.contains("10 worth a double check"), "{c}");
        // Each cluster renders as one counted bullet…
        assert!(c.contains("6 related concerns in svcA@src/a.ts around null/bounds"), "{c}");
        assert!(c.contains("4 related concerns in svcB@src/b.ts around provenance/sibling"), "{c}");
        // …and the section is exactly those two bullets, not ten raw ones.
        assert_eq!(c.matches("\n- ").count(), 2, "clustered section collapsed to two bullets:\n{c}");
        // The raw per-finding note text is NOT walled out line-by-line.
        assert!(!c.contains("`svcA@src/a.ts` — a bounds concern"), "raw bullets suppressed:\n{c}");
    }

    /// Below the threshold (no clusters produced), the "worth a double check"
    /// section renders the RAW per-finding bullets exactly as before — the
    /// clustering path is inert for small tiers.
    #[test]
    fn synthesize_needs_check_below_threshold_renders_raw_bullets() {
        let judged = vec![
            needs_check_flag("svcA@src/a.ts", Some("const b = 2;"), "double check one", false),
            needs_check_flag("svcB@src/b.ts", None, "double check two", false),
        ];
        // needs_check_clusters left empty (Default) — clustering did NOT fire.
        let env = healthy_envelope(judged);
        let r = synthesize_funnel(&env, DIFF, None);
        let c = r.comment.unwrap();
        assert!(c.contains("2 worth a double check"), "{c}");
        assert!(c.contains("double check one"), "raw bullet retained:\n{c}");
        assert!(c.contains("double check two"), "raw bullet retained:\n{c}");
        assert!(!c.contains("related concerns"), "no cluster line when below threshold:\n{c}");
    }

    /// A confirmed finding that ABSORBED same-location duplicate framings at
    /// dedup renders a trailing "Also flagged (same location): …" line, so
    /// the "aggregate, never discard" safety net is VISIBLE on the PR.
    #[test]
    fn synthesize_confirmed_renders_absorbed_also_flagged_framings() {
        let mut j = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "the primary framing of the bug",
            "evidence",
        );
        j.flag.also_flagged =
            vec!["a second framing of the same defect".into(), "a third framing".into()];
        let env = healthy_envelope(vec![j]);
        let r = synthesize_funnel(&env, DIFF, None);
        let body = r.review.unwrap()["comments"][0]["body"].as_str().unwrap().to_string();
        assert!(body.contains("Also flagged (same location):"), "{body}");
        assert!(body.contains("a second framing of the same defect"), "{body}");
        assert!(body.contains("a third framing"), "{body}");
    }

    /// The general (unanchored) confirmed bullet ALSO surfaces absorbed
    /// framings — the safety net holds whether the finding anchored or not.
    #[test]
    fn synthesize_confirmed_general_bullet_surfaces_also_flagged() {
        let mut j = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("this anchor never appears in the diff"),
            "unanchored primary framing",
            "evidence",
        );
        j.flag.also_flagged = vec!["absorbed alternate framing".into()];
        let env = healthy_envelope(vec![j]);
        let r = synthesize_funnel(&env, DIFF, None);
        let body = r.review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(body.contains("Also flagged (same location): absorbed alternate framing"), "{body}");
    }

    // ── #1272: dispatch.start/terminal bookends around a production funnel run ──

    /// Recording [`FunnelEmitter`] mock — pushes every emitted record into a
    /// `Vec` so a test can assert the exact action sequence, same discipline
    /// as `darkmux-lab`'s own `funnel.rs` test-module `RecordingEmitter`
    /// (that one is private to its crate's test module, so this is the
    /// pr_review-layer equivalent — the "smallest injectable seam" the
    /// live-dispatch path needed).
    #[derive(Default)]
    struct RecordingEmitter {
        records: Vec<darkmux_flow::FlowRecord>,
    }
    impl FunnelEmitter for RecordingEmitter {
        fn emit(&mut self, record: darkmux_flow::FlowRecord) {
            self.records.push(record);
        }
    }

    /// A minimal, valid `FlowRecord` standing in for whatever `run_funnel`/
    /// `run_judge_only` would really emit through the injected `FunnelEmitter`
    /// mid-dispatch — the bookend wrapper doesn't inspect these records, only
    /// brackets them, so a bare action string is enough to prove ordering.
    fn fake_funnel_record(action: &str) -> darkmux_flow::FlowRecord {
        darkmux_flow::FlowRecord {
            ts: darkmux_flow::ts_utc_now(),
            level: darkmux_flow::Level::Info,
            category: darkmux_flow::Category::Work,
            tier: darkmux_flow::Tier::Local,
            stage: darkmux_flow::Stage::Dispatch,
            action: action.to_string(),
            handle: "test-crew".to_string(),
            sprint_id: None,
            session_id: Some("case-1".to_string()),
            source: Some("funnel".to_string()),
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        }
    }

    #[test]
    fn with_dispatch_bookends_start_precedes_funnel_records_with_exactly_one_terminal_on_success() {
        let mut emitter = RecordingEmitter::default();
        let result = with_dispatch_bookends(&mut emitter, "case-1", "test-crew", Some("darkmux:judge-model"), |em| {
            em.emit(fake_funnel_record("funnel.task"));
            em.emit(fake_funnel_record("funnel.step"));
            em.emit(fake_funnel_record("funnel.ruling"));
            Ok(FunnelEnvelope::default())
        });
        assert!(result.is_ok());

        let actions: Vec<&str> = emitter.records.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, vec!["dispatch start", "funnel.task", "funnel.step", "funnel.ruling", "dispatch complete"]);

        let start_idx = actions.iter().position(|a| *a == "dispatch start").expect("dispatch start emitted");
        assert!(
            actions.iter().enumerate().filter(|(_, a)| a.starts_with("funnel.")).all(|(i, _)| i > start_idx),
            "dispatch start must precede every funnel.* record: {actions:?}"
        );

        let terminals = actions.iter().filter(|a| **a == "dispatch complete" || **a == "dispatch error").count();
        assert_eq!(terminals, 1, "exactly one terminal dispatch record: {actions:?}");

        let start = &emitter.records[0];
        assert_eq!(start.session_id.as_deref(), Some("case-1"));
        assert_eq!(start.handle, "test-crew");
        assert_eq!(start.model.as_deref(), Some("darkmux:judge-model"));
        assert!(matches!(start.level, darkmux_flow::Level::Info));
        // Provenance: the bookends must carry the SAME source tag as the
        // funnel.* records they bracket — never the builder's hardcoded
        // "crew_dispatch" (which would split one session across two
        // contradictory Source facets in the viewer).
        assert_eq!(start.source.as_deref(), Some("funnel"));

        let terminal = emitter.records.last().unwrap();
        assert_eq!(terminal.action, "dispatch complete");
        assert!(matches!(terminal.level, darkmux_flow::Level::Info));
        assert_eq!(terminal.source.as_deref(), Some("funnel"));
        assert_eq!(terminal.payload.as_ref().unwrap()["result_class"], "ok");
    }

    #[test]
    fn with_dispatch_bookends_error_path_emits_dispatch_error_terminal_with_the_real_message() {
        let mut emitter = RecordingEmitter::default();
        let result = with_dispatch_bookends(&mut emitter, "case-2", "test-crew", None, |em| {
            em.emit(fake_funnel_record("funnel.task"));
            Err(anyhow!("probe dispatch failed: connection refused"))
        });
        assert!(result.is_err());

        let actions: Vec<&str> = emitter.records.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, vec!["dispatch start", "funnel.task", "dispatch error"]);

        let terminals = actions.iter().filter(|a| **a == "dispatch complete" || **a == "dispatch error").count();
        assert_eq!(terminals, 1, "exactly one terminal dispatch record: {actions:?}");

        let terminal = emitter.records.last().unwrap();
        assert_eq!(terminal.action, "dispatch error");
        assert!(matches!(terminal.level, darkmux_flow::Level::Error));
        assert_eq!(terminal.source.as_deref(), Some("funnel"), "error terminal carries funnel provenance");
        let payload = terminal.payload.as_ref().unwrap();
        assert_eq!(payload["result_class"], "error");
        assert!(
            payload["error"].as_str().unwrap().contains("connection refused"),
            "the real error text must survive onto the terminal record: {payload}"
        );
    }

    #[test]
    fn with_dispatch_bookends_panic_still_emits_a_dispatch_error_terminal_via_the_guard() {
        // The RAII headline (mirrors darkmux-crew's `DispatchBookendGuard`
        // #717 coverage): a panic between `dispatch start` and either normal
        // terminal branch must still leave a matching `dispatch error` —
        // never an orphaned start that renders as perpetually running.
        let mut emitter = RecordingEmitter::default();
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // silence the expected panic backtrace
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_dispatch_bookends(&mut emitter, "case-3", "test-crew", None, |_em| {
                panic!("simulated crash mid-dispatch");
            })
        }));
        std::panic::set_hook(prev_hook);
        assert!(result.is_err(), "the wrapped closure should have panicked");

        let actions: Vec<&str> = emitter.records.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, vec!["dispatch start", "dispatch error"]);
        let terminal = emitter.records.last().unwrap();
        assert!(matches!(terminal.level, darkmux_flow::Level::Error));
        assert_eq!(terminal.source.as_deref(), Some("funnel"), "guard's Drop terminal carries funnel provenance");
        assert!(terminal.payload.as_ref().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("early return or panic"));
    }

    #[test]
    fn crew_model_summary_dedupes_and_sorts_every_seat_model() {
        use darkmux_profiles::crews::ResolvedSeatStaffing;
        use darkmux_types::ProfileModel;
        use std::collections::BTreeMap;

        fn staffing(model_id: &str) -> ResolvedSeatStaffing {
            ResolvedSeatStaffing {
                name: "fast".into(),
                pm: ProfileModel { id: model_id.into(), ..Default::default() },
                k: 1,
                passes: 2,
                max_tokens: None,
                selector: None,
            }
        }
        let mut seats = BTreeMap::new();
        seats.insert("review-probe".to_string(), vec![staffing("zzz-probe"), staffing("aaa-probe")]);
        seats.insert("review-judge".to_string(), vec![staffing("zzz-probe")]); // same model as one probe seat
        let crew = ResolvedCrew { name: "test-crew".into(), seats };

        let summary = crew_model_summary(&crew).expect("non-empty crew has a summary");
        assert_eq!(summary, "darkmux:aaa-probe, darkmux:zzz-probe", "sorted + deduped: {summary}");
    }

    #[test]
    fn crew_model_summary_none_for_a_crew_with_no_staffed_seats() {
        let crew = ResolvedCrew { name: "empty-crew".into(), seats: std::collections::BTreeMap::new() };
        assert_eq!(crew_model_summary(&crew), None);
    }
}
