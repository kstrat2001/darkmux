//! (#1060) Synthesize a darkmux review-funnel envelope into a GitHub
//! PR-review payload.
//!
//! The `pr-reviewer` role emits findings that each quote the line they're
//! about (`anchor`) rather than a line *number* (#1053 quote-resolve — local
//! models name the construct reliably but guess its coordinate badly). The
//! coordinate half is deterministic, so it lives here, in the binary, versioned
//! **with** the role's output schema — when the schema changes, this changes
//! with it in one release, instead of every repo updating a copied script that
//! silently degrades when it drifts.
//!
//! The output is one JSON object:
//! `{ "mode": "review"|"comment"|"degraded", "review": <gh-review-payload>|null,
//!    "comment": <markdown>|null }`. The thin workflow YAML posts it (so the
//! operator keeps control of the model/profile, trigger, and the `gh` call).
//! `mode: "degraded"` (#1113) means NO review signal was produced (a
//! degenerate envelope) — the workflow posts the comment AND marks its check
//! failed/neutral, never green.
//!
//! Consumers: `darkmux mission launch review` (`src/mission_launch_review.rs`)
//! drives the `darkmux_lab::lab::review::{build_review_graph, run_review_graph,
//! run_judge_only}` machinery and calls back into
//! [`synthesize_review`]/[`emit_rendered`] here for its own render step.
//! This module's synthesis logic (a `Tier::Confirmed` flag becomes an inline,
//! merge-blocking comment or a general body item when its anchor can't be
//! resolved to a diff line; a `Tier::NeedsCheck` flag becomes a non-blocking
//! "worth a double check" note; a `Tier::Archived` flag never renders) is the
//! shared entry point.
//!
//! (#1426) The top-level `darkmux pr-review` CLI verb — and its
//! single-envelope render path (`cmd_render` / `render_with_attribution`) —
//! retired here: the base tool does not know domain workflows by name, and
//! `mission launch review` already renders the `{mode, review, comment}`
//! payload itself. The review-FUNNEL dispatch (bundle -> probe(k draws) ->
//! dedup -> double-confirm judge -> synthesis) had earlier moved out of this
//! module to `mission launch review` in #1284 Packet 4b.

use anyhow::{Context, Result};
use darkmux_lab::lab::review::{JudgeRecord, ReviewEnvelope, Tier, VerifyRecord, VerifyRuling};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

/// (#1298/#1186) Default tagline for the review's posted footer — the
/// operator-owned "why this comment exists" half. The operator overrides it
/// via `--attribution`; the model / local-vs-cloud half is DERIVED from the
/// run, never carried here (see [`dispatch_provenance`]).
const REVIEW_TAGLINE: &str = "Advisory, not a merge gate.";

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
/// marker on). First non-empty line of a multi-line quote; trimmed match. If the
/// exact whole-line match fails, falls back to a substring match (the review
/// stores a sub-expression span, not the whole line) — but only when exactly one
/// new-side line contains it, so it still never guesses between candidates.
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
    // (#1299) Fragment fallback (the mis-anchor half; dedup half shipped 1.18.1).
    // The review stores a backtick SPAN as the anchor
    // (`extract_new_side_anchor` in the lab crate) — frequently a sub-expression
    // of a changed line, not the whole line — so it matched the diff by SUBSTRING
    // at extraction time but the whole-line lookup above misses it, and the
    // finding wrongly falls to the general section. Recover symmetrically: find
    // the new-side line(s) whose whitespace-collapsed content CONTAINS the
    // collapsed span; anchor only if exactly ONE distinct line matches (never
    // guess between several). A short span is refused so a common fragment can't
    // match broadly. This runs ONLY after the exact lookup fails: a whole-line
    // anchor (the common single-model case) short-circuits above and never
    // reaches here, so no existing resolution changes — only a
    // previously-unresolvable fragment can newly resolve.
    let needle: String = first
        .trim()
        .trim_start_matches(['+', '-', ' '])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if needle.chars().count() >= 8 {
        let mut lines: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for (content, nums) in table {
            if content.split_whitespace().collect::<Vec<_>>().join(" ").contains(&needle) {
                lines.extend(nums.iter().copied());
            }
        }
        if lines.len() == 1 {
            return lines.into_iter().next();
        }
    }
    None
}

/// The degraded rendering with an explicit footer — so the review path can
/// pass its envelope-derived footer (#1298) while the single-reviewer path
/// keeps the static-default footer. Body text is identical either way.
fn degraded_with_footer(note: &str, footer: &str) -> Rendered {
    Rendered {
        mode: "degraded",
        review: None,
        comment: Some(format!(
            "### 🤖 PR review — ⚠️ no review signal\n\n{note} \
             **This is not a clean pass** — the automated reviewer produced \
             no usable review, so this pull request has had no automated \
             review. Human review (or a re-run) required.{footer}"
        )),
    }
}

/// (#1298) The review's posted footer, with its dispatch-provenance clause
/// DERIVED from the envelope's member records — never a hardcoded claim that
/// can contradict the run. The prior static footer asserted "`pr-reviewer`
/// role, running on a local model (no cloud API)" unconditionally; on the
/// first all-remote (Azure) review that was three lies in the most
/// visible place — a public comment saying "no cloud API" about a cloud
/// review (#1186 "never off the meter"). The operator's `--attribution`
/// supplies only the tagline; WHERE the models ran is computed from what
/// actually ran.
fn review_footer(env: &ReviewEnvelope, attribution: Option<&str>) -> String {
    let tagline = attribution
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(REVIEW_TAGLINE);
    format!(
        "\n\n---\n<sub>Automated review — {} — {tagline} \
         · [Powered by darkmux](https://darkmux.com)</sub>",
        dispatch_provenance(env)
    )
}

/// (#1298) The dispatch-provenance clause: WHERE the review's seats actually
/// ran, read from the envelope's member records. An audit-integrity surface —
/// a posted review must never claim "no cloud API" about a review that
/// dispatched to a hosted endpoint.
///
/// - All seats local → "on a local model, no cloud API" (the only honest home
///   for that phrase).
/// - Any seat remote → names the hosted model(s); never "no cloud API".
/// - Mixed crew → names both the local and the hosted seat models.
/// - No member records → asserts neither local nor cloud (stays honest).
fn dispatch_provenance(env: &ReviewEnvelope) -> String {
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
/// preserved. (#1300) An aliased deployment (`model` = the requested/declared
/// id) whose response reported a DIFFERENT `served_model` surfaces BOTH —
/// "requested X, served Y" — never silently hiding the alias behind the
/// deployment name. Agreeing or absent, `model` alone is shown (the common,
/// unremarkable case).
fn unique_seat_models(env: &ReviewEnvelope, remote: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in env.members.iter().filter(|m| m.remote == remote) {
        if m.model.is_empty() {
            continue;
        }
        let name = match m.served_model.as_deref() {
            Some(served) if served != m.model && !served.is_empty() => {
                format!("requested {}, served {served}", m.model)
            }
            _ => m.model.clone(),
        };
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════
// The review dispatch verb (bundle -> probe -> dedup -> judge -> verify ->
// synthesis) retired from here in #1284 Packet 4b — it's now `darkmux
// mission launch review` (`src/mission_launch_review.rs`), which reuses
// `pr_review::{synthesize_review, emit_rendered, Rendered}` below as its
// own render step. This file keeps ONLY `pr-review render` (single-
// envelope render, unchanged) and the synthesis machinery both entry
// points share.
// ═══════════════════════════════════════════════════════════════════════

/// `--emit <path>` writes the rendered payload to a file; `--emit -` or a
/// bare omitted `--emit` writes to stdout (the `-` spelling is what makes
/// a CI-testable `--from-envelope ... --emit -`-style path assertable
/// without a scratch file). `pub(crate)` — `mission_launch_review::launch`
/// calls this as its own final render step.
pub(crate) fn emit_rendered(rendered: &Rendered, emit: Option<&Path>) -> Result<i32> {
    let out = serde_json::to_string(&rendered.to_value())?;
    match emit {
        Some(p) if p == Path::new("-") => println!("{out}"),
        Some(p) => std::fs::write(p, &out).with_context(|| format!("writing {}", p.display()))?,
        None => println!("{out}"),
    }
    Ok(0)
}

/// Real `Bundle.id`s are `"<fn>@<path>"` (`darkmux_lab::lab::bundle::
/// build_bundles`); the provisional review-internal bundler used
/// `id == path` with no `@`. `split_once('@')` — the FIRST `@` — handles
/// both: function names never contain `@`, but paths can (an npm
/// `@scope/pkg` vendored path), so splitting on the LAST `@` would mangle
/// them. Falls back to the whole id when there's no `@` to split on.
fn path_from_bundle_id(bundle_id: &str) -> &str {
    bundle_id.split_once('@').map(|(_, p)| p).unwrap_or(bundle_id)
}

/// (#1222 Phase B packet 5) The fixed marker every `Tier::Confirmed`
/// finding carries. A local judge's double-confirm is real signal (the
/// CLAUDE.md "recheck vs rethink" doctrine's cross-context re-thinking, at
/// judge scale) — but it is not the frontier review the same doctrine
/// reserves for invariant/security-bearing diffs. The marker keeps that
/// distinction visible to whoever reads the posted comment.
const CONFIRMED_MARKER: &str =
    "⚠ needs frontier verification — confirmed by a local judge, not yet frontier-verified";

/// Three-tier synthesis of a [`ReviewEnvelope`] into the [`Rendered`]
/// `{mode, review, comment}` contract:
///
/// - [`Tier::Confirmed`] -> an inline review comment (anchor resolved via
///   [`new_side_index`]/[`resolve_anchor`] against `diff` — the same
///   discipline [`render_with_attribution`] uses) or, when the anchor can't
///   be resolved to exactly one diff line, a general body item — never a
///   guessed line. Confirmed findings render a formal review whose event is
///   NON-blocking `COMMENT` by default (#1302 — advisory, matching the
///   footer's claim; the inline comments are still carried), or blocking
///   `REQUEST_CHANGES` when the crew opts in via `request_changes: true`
///   (read here from `env.staffing`).
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
///   review -> an honest `"comment"` summary naming how much was
///   investigated plus which models ran it — never a silent green pass.
/// - A degenerate envelope (`env.degenerate.is_some()`) -> `"degraded"`,
///   the same contract [`degraded_fallback`] (#1113) uses elsewhere in
///   this module — no review signal was produced.
pub fn synthesize_review(env: &ReviewEnvelope, diff: &str, attribution: Option<&str>) -> Rendered {
    if let Some(note) = &env.degenerate {
        // (#1298) Even a degenerate run posts the envelope-derived footer, so a
        // remote crew that produced no signal never claims "no cloud API".
        return degraded_with_footer(
            &format!("The review produced no signal: {note}."),
            &review_footer(env, attribution),
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
        let mut lines = vec!["### 🤖 PR review".to_string(), String::new()];
        if needs_check_count == 0 {
            lines.push(format!(
                "review ran: {} flags investigated across {} bundles, none confirmed. _{}_",
                env.deduped_flags,
                env.bundles,
                member_summary(env)
            ));
        } else {
            lines.push(format!(
                "review ran: {} flags investigated across {} bundles, none confirmed — \
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
            comment: Some(format!("{}{}", lines.join("\n"), review_footer(env, attribution))),
        };
    }

    let mut body = vec!["### 🤖 PR review".to_string(), String::new()];
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

    // (#1302) Confirmed findings default to a NON-blocking `COMMENT`-event
    // formal review: it keeps the formal-review structure and the inline
    // `comments` on confirmed findings, but does NOT set GitHub's
    // `reviewDecision` to `CHANGES_REQUESTED`, so it never blocks merge —
    // matching the footer's "advisory, not a merge gate" claim. A crew opts
    // back into blocking via `request_changes: true` (snapshotted onto the
    // envelope's staffing), which renders `REQUEST_CHANGES`. Blocking mode
    // has no automated resolution path yet — darkmux can't supersede its own
    // block; #1260 (`review-verify`) + a re-supersede-on-clean-rerun step are
    // the prerequisites before it should ever default on.
    let event = if env.staffing.as_ref().is_some_and(|s| s.request_changes) {
        "REQUEST_CHANGES"
    } else {
        "COMMENT"
    };

    Rendered {
        mode: "review",
        review: Some(json!({
            "event": event,
            "body": format!("{}{}", body.join("\n"), review_footer(env, attribution)),
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
fn run_warnings_block(env: &ReviewEnvelope) -> Vec<String> {
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
/// [`footer_for`]). Names WHICH local models ran the review, not WHERE it
/// ran.
fn member_summary(env: &ReviewEnvelope) -> String {
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
    // Test-only: these review types aren't referenced by this module's
    // production code (only inferred through `env.judged`/`env.members`
    // iteration), so importing them at file scope would warn "unused" on a
    // plain (non-test) `cargo build`.
    use darkmux_lab::lab::review::{JudgeRuling, JudgedFlag, MemberRecord, NeedsCheckCluster, ProbeFlag};

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

    // ── #1300 fragment fallback: the review stores a sub-expression SPAN,
    //    not the whole line, so the exact lookup misses. Recover via a
    //    substring match, but only when exactly one new-side line contains it.

    #[test]
    fn resolve_span_fragment_recovers_the_unique_line() {
        // `const b =` is a fragment of line 2's content only -> resolves to 2.
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("const b ="), &idx()), Some(2));
        // whole-line still resolves via the exact path (behavior unchanged).
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("const b = 2;"), &idx()), Some(2));
    }

    #[test]
    fn resolve_short_fragment_is_refused() {
        // Below the 8-char floor -> no broad guessing.
        assert_eq!(resolve_anchor(Some("src/x.ts"), Some("b = 2;"), &idx()), None);
    }

    #[test]
    fn resolve_fragment_on_multiple_lines_stays_none() {
        let d = "diff --git a/g.ts b/g.ts\n--- a/g.ts\n+++ b/g.ts\n@@ -1,2 +1,2 @@\n+const a = wrapValue(input);\n+const b = wrapValue(other);\n";
        let i = new_side_index(d);
        // `wrapValue(` is on BOTH added lines -> ambiguous -> None (never guess).
        assert_eq!(resolve_anchor(Some("g.ts"), Some("wrapValue("), &i), None);
    }

    // ── extract/render: tolerant JSON extraction ──────────────────────────

    // ── render: payload shape ─────────────────────────────────────────────

    // ── cmd_render handler ────────────────────────────────────────────────

    // ─── #1113: vacuous-pass gate ──────────────────────────────────────

    // ─── #1113: freeform marker contract (pr-reviewer-agentic) ────────

    // ─── #1113: footer attribution ─────────────────────────────────────

    // ─── synthesize_review: three-tier synthesis (#1222 Phase B packet 5) ──

    fn judge_record(ruling: JudgeRuling, evidence: &str, note: &str) -> JudgeRecord {
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
    /// only way `judge_one_flag` (review.rs) ever produces `Tier::Confirmed`.
    fn confirmed_flag(bundle_id: &str, anchor: Option<&str>, note: &str, evidence: &str) -> JudgedFlag {
        let record = judge_record(JudgeRuling::Confirmed, evidence, note);
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
    /// pass-2 at all. Either way `tier` is `NeedsCheck` — `synthesize_review`
    /// doesn't special-case `demoted_by_pass2` itself, it just reads `tier`.
    fn needs_check_flag(bundle_id: &str, anchor: Option<&str>, note: &str, demoted: bool) -> JudgedFlag {
        if demoted {
            JudgedFlag {
                flag: probe_flag(bundle_id, anchor),
                pass1: judge_record(JudgeRuling::Confirmed, "pass-1 evidence", "pass-1 note"),
                pass2: Some(judge_record(JudgeRuling::FalsePositive, "pass-2 evidence", note)),
                tier: Tier::NeedsCheck,
                demoted_by_pass2: true,
                    verify: None,
                    demoted_by_verify: false,
            }
        } else {
            JudgedFlag {
                flag: probe_flag(bundle_id, anchor),
                pass1: judge_record(JudgeRuling::NeedsCheck, "pass-1 evidence", note),
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
            pass1: judge_record(JudgeRuling::FalsePositive, "not a real issue", "no action needed"),
            pass2: None,
            tier: Tier::Archived,
            demoted_by_pass2: false,
                verify: None,
                demoted_by_verify: false,
        }
    }

    fn healthy_envelope(judged: Vec<JudgedFlag>) -> ReviewEnvelope {
        let distinct_bundles: std::collections::HashSet<&str> =
            judged.iter().map(|j| j.flag.bundle_id.as_str()).collect();
        ReviewEnvelope {
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

    /// (#1302) A healthy envelope whose staffing snapshot opts into the
    /// blocking review event (`request_changes: true`) — the mirror of the
    /// default (`healthy_envelope`, which leaves `staffing: None` and so
    /// renders the non-blocking `COMMENT`).
    fn blocking_envelope(judged: Vec<JudgedFlag>) -> ReviewEnvelope {
        ReviewEnvelope {
            staffing: Some(darkmux_lab::lab::review::StaffingSnapshot {
                request_changes: true,
                ..Default::default()
            }),
            ..healthy_envelope(judged)
        }
    }

    #[test]
    fn synthesize_confirmed_resolves_inline_with_marker_and_comment_by_default() {
        let j = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "shadows the config default",
            "the clamp is bypassed",
        );
        let env = healthy_envelope(vec![j]);
        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "review");
        let review = r.review.unwrap();
        // (#1302) Default: a formal review with the NON-blocking `COMMENT`
        // event — it never sets `reviewDecision: CHANGES_REQUESTED`, so it
        // never blocks merge, yet still carries its inline comments.
        assert_eq!(review["event"], "COMMENT", "a confirmed finding is advisory by default");
        let comments = review["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1, "the COMMENT-event review still carries its inline finding");
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

    /// (#1302) Opt-in blocking: `request_changes: true` on the crew (carried
    /// onto the envelope's staffing snapshot) restores the formal blocking
    /// `REQUEST_CHANGES` event — the inline findings are unchanged.
    #[test]
    fn synthesize_confirmed_request_changes_opt_in_renders_blocking_event() {
        let j = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "shadows the config default",
            "the clamp is bypassed",
        );
        let env = blocking_envelope(vec![j]);
        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "review");
        let review = r.review.unwrap();
        assert_eq!(review["event"], "REQUEST_CHANGES", "request_changes:true opts back into blocking");
        assert_eq!(
            review["comments"].as_array().unwrap().len(),
            1,
            "the blocking review carries the same inline finding as the advisory default"
        );
    }

    /// (#1302) A confirmed finding whose anchor can't resolve lands in the
    /// review BODY — and the default event stays the non-blocking `COMMENT`
    /// (the general-finding path picks the event the same way the inline path
    /// does).
    #[test]
    fn synthesize_confirmed_general_finding_is_comment_by_default() {
        let j = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("this text never appears in the diff"),
            "a note the judge left",
            "some evidence",
        );
        let env = healthy_envelope(vec![j]);
        let r = synthesize_review(&env, DIFF, None);
        let review = r.review.unwrap();
        assert_eq!(review["event"], "COMMENT", "general confirmed findings are advisory by default too");
        assert_eq!(review["comments"].as_array().unwrap().len(), 0, "unresolvable anchor never guesses a line");
        assert!(review["body"].as_str().unwrap().contains("not anchored to a diff line"));
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
        let r = synthesize_review(&env, DIFF, None);
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
        let r = synthesize_review(&env, DIFF, None);
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
        let r = synthesize_review(&env, DIFF, None);
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
        let r = synthesize_review(&env, DIFF, None);
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
        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "comment", "zero confirms never opens a review");
        let c = r.comment.unwrap();
        assert!(c.contains("worth a double check"), "{c}");
        assert!(c.contains("double check this one"), "{c}");
    }

    #[test]
    fn synthesize_zero_confirms_zero_needs_check_healthy_is_honest_comment() {
        let env = ReviewEnvelope {
            deduped_flags: 3,
            bundles: 2,
            judged: vec![archived_flag("computeEnd@src/x.ts"), archived_flag("otherFn@src/y.ts")],
            members: vec![
                MemberRecord { model: "darkmux:probe-a".into(), seat: "review-probe".into(), ..Default::default() },
                MemberRecord { model: "darkmux:judge-b".into(), seat: "review-judge".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "comment");
        let c = r.comment.unwrap();
        assert!(
            c.contains("review ran: 3 flags investigated across 2 bundles, none confirmed"),
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
        let r = synthesize_review(&env, DIFF, None);
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
        let env = ReviewEnvelope {
            degenerate: Some("zero flags from all probe draws — never a silent pass".to_string()),
            ..Default::default()
        };
        let r = synthesize_review(&env, DIFF, None);
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
        // review): the envelope finish_review now produces when every judge
        // ruling was Unparsed/Error — judged flags all Archived, zero
        // confirms/needs-check, degenerate SET — must route to "degraded",
        // never to the honest "N flags investigated, none confirmed"
        // comment.
        let mut env = healthy_envelope(vec![JudgedFlag {
            flag: probe_flag("computeEnd@src/x.ts", None),
            pass1: judge_record(JudgeRuling::Unparsed, "", ""),
            pass2: None,
            tier: Tier::Archived,
            demoted_by_pass2: false,
                verify: None,
                demoted_by_verify: false,
        }]);
        env.degenerate =
            Some("judge produced no usable ruling on any of 1 flags (all errored/unparsed)".to_string());
        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "degraded", "a dead judge must never render green");
        let c = r.comment.unwrap();
        assert!(c.contains("no usable ruling"), "{c}");
        assert!(!c.contains("none confirmed"), "must not read like an honest pass: {c}");
    }

    #[test]
    fn synthesize_attribution_flows_into_footer() {
        let confirmed =
            confirmed_flag("computeEnd@src/x.ts", Some("const b = 2;"), "n", "e");
        let att = "Reviewed by the review review on the repo's self-hosted runner.";
        let env = healthy_envelope(vec![confirmed]);
        let r = synthesize_review(&env, DIFF, Some(att));
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
        let env = ReviewEnvelope { members, ..healthy_envelope(vec![confirmed]) };
        synthesize_review(&env, DIFF, None).review.unwrap()["body"]
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
        assert!(body.contains("Powered by darkmux"), "names the crew/review instead: {body}");
    }

    // ─── #1300: served model surfaces an aliased deployment, never hides it ─

    #[test]
    fn footer_aliased_deployment_names_both_requested_and_served() {
        let mut judge = judge_member("gpt-4o", true);
        judge.served_model = Some("gpt-4o-2026-08-01".to_string());
        let body = footer_body_for(vec![probe_member("gpt-4o", true), judge]);
        assert!(
            body.contains("requested gpt-4o, served gpt-4o-2026-08-01"),
            "an aliased deployment must name both, never hide the alias: {body}"
        );
    }

    #[test]
    fn footer_served_model_matching_requested_shows_just_the_name() {
        let mut judge = judge_member("gpt-4o", true);
        judge.served_model = Some("gpt-4o".to_string());
        let body = footer_body_for(vec![probe_member("gpt-4o", true), judge]);
        assert!(!body.contains("requested"), "agreement is unremarkable, no aliasing callout: {body}");
        assert!(body.contains("gpt-4o"), "{body}");
    }

    #[test]
    fn footer_served_model_absent_shows_just_the_requested_name() {
        // A local seat, or a remote endpoint whose response omitted `model` —
        // absence is never treated as "matches", but it's also never treated
        // as "differs"; just fall back to the requested id, unremarkably.
        let body = footer_body_for(vec![probe_member("gpt-4o", true), judge_member("gpt-4o", true)]);
        assert!(!body.contains("requested"), "no served_model captured -> no aliasing callout: {body}");
        assert!(body.contains("gpt-4o"), "{body}");
    }

    // ─── synthesize_review: coverage sweep (#1222 Phase B packet 5 QA) ────

    #[test]
    fn synthesize_confirmed_ambiguous_anchor_falls_to_general() {
        // The `hits.len() == 1` discipline (`resolve_anchor`) must hold at
        // the synthesize_review level too: a confirmed flag whose anchor
        // matches TWO diff lines must never guess — it lands in the general
        // body list, same as an unresolvable anchor.
        let dup_diff = "diff --git a/y.ts b/y.ts\n+++ b/y.ts\n@@ -1,0 +1,2 @@\n+  return;\n+  return;\n";
        let j = confirmed_flag("fn@y.ts", Some("return;"), "ambiguous note", "ambiguous evidence");
        let env = healthy_envelope(vec![j]);
        let r = synthesize_review(&env, dup_diff, None);
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
        let record = judge_record(JudgeRuling::Confirmed, "", "");
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
        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "review", "a verified finding is still merge-blocking");
        let review = r.review.unwrap();
        let body = review["comments"][0]["body"].as_str().unwrap();
        assert!(body.contains("verified by gpt-5.1 adjudication"), "{body}");
        assert!(!body.contains(CONFIRMED_MARKER), "the manual-verification marker must be gone: {body}");

        // General (anchor unresolvable) — same replacement.
        let mut g = confirmed_flag("ghost@src/nowhere.ts", Some("no such line"), "note", "evidence");
        g.verify = Some(verify_record(VerifyRuling::Verified));
        let env = healthy_envelope(vec![g]);
        let r = synthesize_review(&env, DIFF, None);
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
        let with_uncertain = synthesize_review(&env, DIFF, None);

        let no_seat = confirmed_flag("computeEnd@src/x.ts", Some("const b = 2;"), "note", "evidence");
        let env2 = healthy_envelope(vec![no_seat]);
        let without_seat = synthesize_review(&env2, DIFF, None);

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

        let r = synthesize_review(&env, DIFF, None);
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
        let r = synthesize_review(&env, DIFF, None);
        let review = r.review.unwrap();
        let body = review["body"].as_str().unwrap();
        let comments = serde_json::to_string(&review["comments"]).unwrap();
        assert!(!body.contains("refuted note") && !comments.contains("refuted note"));
        assert!(comments.contains("kept note"), "the surviving confirm still posts");
    }

    #[test]
    fn confirmed_general_bullet_empty_note_and_evidence_uses_fallback_text() {
        let record = judge_record(JudgeRuling::Confirmed, "", "");
        let line = confirmed_general_bullet("src/x.ts", &record, None, &[]);
        assert!(line.contains("(no note from the judge)"), "{line}");
        assert!(!line.contains("_Evidence:"), "empty evidence must not render a line: {line}");
        assert!(line.contains(CONFIRMED_MARKER), "{line}");
    }

    #[test]
    fn needs_check_bullet_empty_note_uses_fallback_text() {
        let record = judge_record(JudgeRuling::NeedsCheck, "some evidence", "");
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
        let r = synthesize_review(&env, DIFF, None);
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
        // first-`@` fix must hold end-to-end through synthesize_review — a
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
        let r = synthesize_review(&env, scoped_diff, None);
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
        let r = synthesize_review(&env, DIFF, None);
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
        let r = synthesize_review(&env, DIFF, None);
        let c = r.comment.unwrap();
        assert!(c.contains("darkmux:probe-model"), "{c}");
        assert!(c.contains("darkmux:judge-model"), "{c}");
    }

    #[test]
    fn synthesize_honest_zero_comment_names_multiple_probe_models() {
        let env = ReviewEnvelope {
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
        let r = synthesize_review(&env, DIFF, None);
        let c = r.comment.unwrap();
        assert!(c.contains("probed by darkmux:probe-a, darkmux:probe-b"), "{c}");
        assert!(c.contains("judged by darkmux:judge-c"), "{c}");
    }

    #[test]
    fn synthesize_honest_zero_comment_falls_back_to_unknown_with_no_members() {
        let env = ReviewEnvelope {
            deduped_flags: 1,
            bundles: 1,
            judged: vec![archived_flag("a@f1.ts")],
            members: vec![],
            ..Default::default()
        };
        let r = synthesize_review(&env, DIFF, None);
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
        let env = ReviewEnvelope {
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
        let r = synthesize_review(&env, DIFF, None);
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
        let r = synthesize_review(&env, DIFF, None);
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
        let r = synthesize_review(&env, DIFF, None);
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
        let r = synthesize_review(&env, DIFF, None);
        let body = r.review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(body.contains("Also flagged (same location): absorbed alternate framing"), "{body}");
    }
}
