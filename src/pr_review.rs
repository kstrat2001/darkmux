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
//! `{ "mode": "review"|"comment"|"partial"|"degraded"|"noop", "review": <gh-review-payload>|null,
//!    "comment": <markdown>|null }`. The thin workflow YAML posts it (so the
//! operator keeps control of the model/profile, trigger, and the `gh` call).
//! `mode: "degraded"` (#1113) means NO review signal was produced (a
//! degenerate envelope) — the workflow posts the comment AND marks its check
//! failed/neutral, never green. `mode: "noop"` (#1605) is a DIFFERENT
//! outcome from `"degraded"`: a genuinely non-code diff (every file the
//! bundler touched declined for a benign reason) posts a neutral note
//! naming what the diff contained — not a failure, not a green approval.
//! `mode: "partial"` (#1876/#1877) is a FOURTH outcome, distinct from all
//! three: the judge stage did not rule on every flag (its remote token
//! budget exhausted mid-docket), but everything it DID rule on is real,
//! posted signal — the `review`/`comment` payload renders exactly as it
//! would on a complete run, plus a prominent banner naming the shortfall.
//! Never a clean, green pass (the workflow fails the run after posting, the
//! same way `"degraded"` does) and never a discard (unlike `"degraded"`,
//! the findings post). Production incident this exists to fix: a judge that
//! had ruled 123 of 134 flags (7 confirmed, 67 needs-check, both complete
//! with evidence) got posted as "the review produced no signal" and the
//! findings were never rendered, because 11 skipped calls tripped the SAME
//! gate a fully-dead judge would.
//!
//! **`comment` in `review` mode is the FALLBACK, not the default action**
//! (#1583). `mode` alone decides what to post; in `review` mode the workflow
//! posts `review` and only reaches for `comment` if GitHub rejects it. That
//! rejection is routine, not exotic: a review takes tens of minutes, and a
//! branch that moves under it invalidates the inline anchors (`422
//! Unprocessable Entity — Line could not be resolved`). Before #1583 that
//! 422 killed the posting step under `set -e` and the ENTIRE review was
//! discarded — a run with eight confirmed findings, one of them a real
//! inverted-comparator bug, left no trace on the PR. A summary-only post is
//! a DEGRADED SUCCESS: the findings reach the author, and the note in the
//! body says anchoring was unavailable so it can't be misread as a thin
//! review.
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
use darkmux_crew::run_outcome::RunOutcome;
use darkmux_lab::lab::bundle::{SkipReason, SkippedFile};
use darkmux_lab::lab::review::{DegenerateKind, JudgeRecord, ReviewEnvelope, Tier, VerifyRecord, VerifyRuling};
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
    pub mode: &'static str, // "review" | "comment" | "partial" (#1876/#1877: coverage shortfall, real signal still posts) | "degraded" (#1113: no review signal) | "noop" (#1605: benign-empty, not a failure)
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

/// (#1605) The neutral no-op comment for a benign-empty run — every file
/// the diff touched declined for a reason that means "nothing here to
/// review" (today: [`darkmux_lab::lab::review::DegenerateKind::BenignEmpty`]
/// fires only when every skip is non-code content — see
/// `classify_zero_bundle_degenerate`'s own doc). Names what the diff
/// actually contained (from `env.bundle_skip`) so the requester sees WHY,
/// never just a bare "nothing to review" — and this is explicitly NOT a
/// green approval: the workflow posts it via `mode: "noop"`, distinct from
/// both `"review"`/`"comment"` (real signal) and `"degraded"` (a genuine
/// failure).
fn render_benign_noop_comment(env: &ReviewEnvelope, footer: &str) -> String {
    let (considered, listing) = match &env.bundle_skip {
        Some(report) => {
            let mut paths: Vec<&str> = report.files_skipped.iter().map(|f| f.path.as_str()).collect();
            paths.sort_unstable();
            const MAX_NAMED: usize = 8;
            let listing = if paths.is_empty() {
                "no files".to_string()
            } else if paths.len() <= MAX_NAMED {
                paths.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ")
            } else {
                format!(
                    "{}, and {} more",
                    paths[..MAX_NAMED].iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", "),
                    paths.len() - MAX_NAMED
                )
            };
            (report.files_considered, listing)
        }
        None => (0, "no files".to_string()),
    };
    format!(
        "### 🤖 PR review — no reviewable bundle\n\n\
         **The bundler ran and worked as expected. This is not a failure, and re-running will \
         produce the same result.**\n\n\
         This diff touched {considered} file(s) — {listing} — and none of them produced a \
         darkmux-reviewable bundle. The bundler covers TypeScript source and deliberately \
         excludes data (fixtures, lockfiles, generated config) and test files, so there was \
         nothing here for an automated code review to read.\n\n\
         **This is a neutral note, not an approval** — it reflects what the diff contained, \
         not a judgment on the change.{footer}"
    )
}

/// (#1757) The neutral note for a run whose zero-bundle result is real
/// source code the built-in bundler can't parse
/// ([`darkmux_lab::lab::review::DegenerateKind::UnsupportedLanguage`] —
/// see `classify_zero_bundle_degenerate`'s doc for exactly which skip
/// mixes qualify). Deliberately NOT [`render_benign_noop_comment`]'s
/// wording: that comment says "nothing here to review," which is false
/// about a `.sql`-only or `.css`-only PR — there IS code here, darkmux's
/// TypeScript-only built-in bundler just doesn't read it. Names the file
/// count and extensions that went unreviewed and points at the
/// `--bundler` escape hatch's own guide page, so the requester's next
/// step is a link, not a guess. Posted via `mode: "noop"` (same as the
/// benign case) — a real neutral outcome, never a failed check and never
/// a green approval.
fn render_unsupported_language_comment(env: &ReviewEnvelope, footer: &str) -> String {
    let (considered, unsupported_paths) = match &env.bundle_skip {
        Some(report) => {
            let mut paths: Vec<&str> = report
                .files_skipped
                .iter()
                .filter(|f| f.reason == SkipReason::SourceLanguageUnsupported)
                .map(|f| f.path.as_str())
                .collect();
            paths.sort_unstable();
            (report.files_considered, paths)
        }
        None => (0, Vec::new()),
    };
    let mut extensions: Vec<String> = unsupported_paths
        .iter()
        .filter_map(|p| p.rsplit_once('.').map(|(_, ext)| format!(".{ext}")))
        .collect();
    extensions.sort_unstable();
    extensions.dedup();
    let ext_list = if extensions.is_empty() { "unknown extension".to_string() } else { extensions.join(", ") };
    const MAX_NAMED: usize = 8;
    let listing = if unsupported_paths.is_empty() {
        "no files".to_string()
    } else if unsupported_paths.len() <= MAX_NAMED {
        unsupported_paths.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ")
    } else {
        format!(
            "{}, and {} more",
            unsupported_paths[..MAX_NAMED].iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", "),
            unsupported_paths.len() - MAX_NAMED
        )
    };
    let n = unsupported_paths.len();
    format!(
        "### 🤖 PR review — unsupported source language\n\n\
         **The bundler ran and worked as expected. This is not a failure, and re-running will \
         produce the same result.**\n\n\
         This diff touched {considered} file(s). {n} of them ({ext_list}) are real source code \
         that darkmux's built-in bundler can't parse — it reads TypeScript only: {listing}.\n\n\
         darkmux parses TypeScript natively. To review other languages, bring your own bundler: \
         see the [bundler guide](https://darkmux.com/guide/bundlers.html).\n\n\
         **This is a neutral note, not an approval** — it reflects what the diff contained, \
         not a judgment on the change.{footer}"
    )
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
    // (#1676) The provenance clause is present only when the envelope carries
    // evidence for it. With no member records there is nothing to say about
    // where anything ran, so the clause drops out entirely rather than being
    // filled with a default — which is what "asserts neither local nor cloud"
    // has to mean on an audit surface.
    match dispatch_provenance(env) {
        Some(provenance) => format!(
            "\n\n---\n<sub>Automated review — {provenance} — {tagline} \
             · [Powered by darkmux](https://darkmux.com)</sub>"
        ),
        None => format!(
            "\n\n---\n<sub>Automated review — {tagline} \
             · [Powered by darkmux](https://darkmux.com)</sub>"
        ),
    }
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
/// - No member records → **`None`**: the clause is omitted, asserting nothing.
///
/// (#1676) That last arm used to return the literal `"on a self-hosted
/// runner"` — a hardcoded claim about the execution environment, made in the
/// one branch where the envelope holds no evidence that anything executed at
/// all. The doc above already said it "asserts neither local nor cloud"; the
/// code didn't. A v2.5.0 release dogfood launched `mission launch review` from
/// a laptop shell and got a public-facing footer saying it ran on a
/// self-hosted runner.
///
/// It survived #1298 (which fixed the three evidence-derived arms) because the
/// no-dispatch path was rare — until #1605 made benign-empty a normal outcome
/// that POSTS a comment. A rare branch became a common one and its content was
/// never re-read. The other three arms are derived from member records and are
/// unchanged.
fn dispatch_provenance(env: &ReviewEnvelope) -> Option<String> {
    let local = unique_seat_models(env, false);
    let remote = unique_seat_models(env, true);
    match (local.is_empty(), remote.is_empty()) {
        (true, true) => None,
        (false, true) => Some(format!("on a local model, no cloud API ({})", local.join(", "))),
        (true, false) => Some(format!("via a hosted cloud endpoint ({})", remote.join(", "))),
        (false, false) => Some(format!(
            "on a mixed crew (local: {}; hosted cloud endpoint: {})",
            local.join(", "),
            remote.join(", ")
        )),
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

/// (#1222 Phase B packet 5; moved to the review header, not repeated per
/// finding, in the concise-review pass) The fixed marker surfaced ONCE, on
/// the review's verdict line, whenever at least one `Tier::Confirmed`
/// finding in the run has no `verified` adjudication. A local judge's
/// double-confirm is real signal (the CLAUDE.md "recheck vs rethink"
/// doctrine's cross-context re-thinking, at judge scale) — but it is not
/// the frontier review the same doctrine reserves for invariant/security-
/// bearing diffs. Individual findings that DO carry a verify adjudication
/// show [`verified_line`] instead, inline with that finding — this marker
/// itself never repeats per finding; see `any_unverified` in
/// [`synthesize_review`].
const CONFIRMED_MARKER: &str =
    "⚠ needs frontier verification — confirmed by a local judge, not yet frontier-verified";

/// (#1530) Bound what a degenerate note may carry into a PUBLIC PR comment.
///
/// `env.degenerate` is composed from each errored step's own `Step.output` —
/// an anyhow chain. Since bundling moved into the graph, that chain can carry
/// the runner's absolute home path, the operator's worktree layout, and (for
/// `--bundler`) arbitrary stderr from a third-party plugin, including a panic
/// backtrace. `synthesize_review`'s output is posted with `gh pr comment` on
/// a repo that may be public, so this is the one boundary in the pipeline
/// where an internal error string crosses into the open.
///
/// The full chain is preserved where it's useful for diagnosis — the step's
/// persisted `output`, the envelope, the flow records, and the local stderr
/// line. Only the comment is trimmed: first line, home-redacted, capped.
///
/// Two honest limits, so this isn't mistaken for a guarantee:
///
/// - This is NOT the only path to a public surface. `darkmux-review.yml`
///   uploads `envelope.json` — which carries `degenerate` verbatim — as an
///   `actions/upload-artifact` on `if: always()`. That predates this
///   function and is unchanged by it; sanitizing here narrows the most-read
///   surface (the comment), it does not close the class.
/// - Redaction covers `$HOME` only. A path under `/tmp`, a macOS
///   `/private/var/folders/…` tempdir, or another user's home survives
///   inside the first 300 characters. Mitigation, not elimination.
fn public_safe_note(note: &str) -> String {
    let first = note.lines().next().unwrap_or("").trim();
    let redacted = match dirs::home_dir() {
        Some(home) => first.replace(&home.display().to_string(), "~"),
        None => first.to_string(),
    };
    const MAX: usize = 300;
    if redacted.chars().count() > MAX {
        let mut clipped: String = redacted.chars().take(MAX).collect();
        clipped.push_str("… (full detail in the run's envelope and logs)");
        clipped
    } else {
        redacted
    }
}

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
///   (read here from `env.staffing`). When one or more confirmed findings
///   lack a `verified` adjudication, [`CONFIRMED_MARKER`] renders ONCE, on
///   the verdict line — never repeated per finding (a verified finding
///   instead names its adjudicator inline, via [`verified_line`]).
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
/// - A degenerate envelope whose `degenerate_kind` is
///   [`DegenerateKind::BenignEmpty`] (#1605 — every file the diff touched
///   declined for a benign, "nothing to review" reason) -> `"noop"`: a short,
///   NEUTRAL comment naming what the diff contained and why there's nothing
///   to review. Not a green approval, not a red failure — see
///   [`render_benign_noop_comment`].
/// - A degenerate envelope whose `degenerate_kind` is
///   [`DegenerateKind::UnsupportedLanguage`] (#1757 — real source code in a
///   language the built-in, TypeScript-only bundler can't parse) -> also
///   `"noop"`, but with different wording: there IS code here, so the
///   comment names the file count + extensions that went unreviewed and
///   points at the `--bundler` escape hatch's guide page instead of saying
///   "nothing to review" — see [`render_unsupported_language_comment`].
/// - An envelope whose [`darkmux_lab::lab::review::review_outcome`] reads
///   [`RunOutcome::Empty`] (`env.degenerate.is_some()`, `degenerate_kind`
///   absent or [`DegenerateKind::Error`] — Gate 2's zero-usable-rulings
///   honesty gate, or the strict judge-exhaustion policy) -> `"degraded"`,
///   via [`degraded_with_footer`] (#1113) — no review signal was produced.
/// - (#1876/#1877) An envelope whose `review_outcome` reads
///   [`RunOutcome::Partial`] (a judge-stage remote token budget exhausted
///   before the whole docket was judged, but usable rulings exist) ->
///   `"partial"`. Renders EXACTLY the `"review"`/`"comment"` body it would
///   on a Complete run — same findings, same fallback — plus a prominent
///   banner at the top ([`render_partial_coverage_banner`]) naming the
///   shortfall in the envelope's own numbers. Never the discard `"degraded"`
///   uses (the judged flags are real signal) and never a clean pass either
///   (`mode` alone tells the workflow to fail the run after posting).
pub fn synthesize_review(env: &ReviewEnvelope, diff: &str, attribution: Option<&str>) -> Rendered {
    if env.degenerate_kind == Some(DegenerateKind::BenignEmpty) {
        // (#1605) A genuinely non-code diff is an honest outcome, not a
        // failure — post a neutral note (never a green approval) instead of
        // the loud "no signal" degraded treatment, and don't fail the run.
        return Rendered {
            mode: "noop",
            review: None,
            comment: Some(render_benign_noop_comment(env, &review_footer(env, attribution))),
        };
    }
    if env.degenerate_kind == Some(DegenerateKind::UnsupportedLanguage) {
        // (#1757) Real source the built-in bundler can't parse is not the
        // same finding as "nothing here" — stay neutral (never fail the
        // run) but name what went unreviewed and point at `--bundler`.
        return Rendered {
            mode: "noop",
            review: None,
            comment: Some(render_unsupported_language_comment(env, &review_footer(env, attribution))),
        };
    }
    // (#1876/#1877) `review_outcome` is review's own predicate mapping onto
    // the generic `RunOutcome` — `Empty` here is EXACTLY the condition the
    // old `if let Some(note) = &env.degenerate` check used (Gate 2's
    // zero-usable-rulings honesty gate, or the strict judge-exhaustion
    // policy opting back into the pre-#1876 behavior); the "produced no
    // signal" wording and the full discard are reserved for it alone.
    let outcome = darkmux_lab::lab::review::review_outcome(env);
    if let RunOutcome::Empty { reason } = &outcome {
        // (#1298) Even a degenerate run posts the envelope-derived footer, so a
        // remote crew that produced no signal never claims "no cloud API".
        return degraded_with_footer(
            &format!("The review produced no signal: {}.", public_safe_note(reason)),
            &review_footer(env, attribution),
        );
    }
    // `Partial` names a judge-stage coverage shortfall (#1876: a remote
    // token budget exhausted before the whole docket was judged, but usable
    // rulings exist) — the findings below still render normally; this only
    // adds a prominent, never-omittable banner and swaps the posted `mode`
    // so the workflow can refuse to read it as a clean, green pass. See
    // `render_partial_coverage_banner`.
    let partial_reasons: Vec<String> = match outcome {
        RunOutcome::Partial { reasons } => reasons,
        RunOutcome::Complete | RunOutcome::Empty { .. } => Vec::new(),
    };
    let partial = !partial_reasons.is_empty();

    let index = new_side_index(diff);
    let mut inline: Vec<Value> = Vec::new();
    let mut confirmed_general: Vec<String> = Vec::new();
    // (#1583) EVERY confirmed finding as a prose bullet — the anchored ones
    // too, which `confirmed_general` deliberately excludes. This is the
    // summary-only FALLBACK body: when GitHub rejects the inline anchors
    // (a 422, typically because the branch moved during a review that takes
    // tens of minutes), the workflow posts this instead of discarding the
    // run. Built here rather than in the workflow YAML because rendering is
    // the binary's job and versioned with the role schema (#1060) — the YAML
    // stays a thin poster.
    let mut confirmed_all: Vec<String> = Vec::new();
    let mut needs_check_lines: Vec<String> = Vec::new();
    // (#1521-adjacent UX) Whether any confirmed finding in this run lacks a
    // `verified` adjudication — drives the ONE header-level [`CONFIRMED_MARKER`]
    // line, replacing what used to be a per-finding repeat.
    let mut any_unverified = false;

    for j in &env.judged {
        let path = path_from_bundle_id(&j.flag.bundle_id);
        match j.tier {
            Tier::Confirmed => {
                let record = j.pass2.as_ref().unwrap_or(&j.pass1);
                // (#1260) A verify-seat `verified` ruling names the
                // adjudicator inline on this finding; `uncertain`/
                // `unparsed`/`error` (or no verify seat at all) leave the
                // finding unverified — an inconclusive adjudication never
                // promotes. `refuted` never reaches here (tier = Archived).
                let verified = j.verify.as_ref().filter(|v| v.ruling == VerifyRuling::Verified);
                if verified.is_none() {
                    any_unverified = true;
                }
                // (#1583) Every confirmed finding gets a prose bullet for the
                // fallback body, anchored or not — an unpostable inline
                // comment must still reach the author. Rendered once and
                // shared with the general list below, which needs the
                // identical string for the unanchored case.
                let bullet =
                    confirmed_general_bullet(path, record, verified, &j.flag.also_flagged);
                confirmed_all.push(bullet.clone());
                match resolve_anchor(Some(path), j.flag.anchor.as_deref(), &index) {
                    Some(line) => inline.push(json!({
                        "path": norm_path(path),
                        "line": line,
                        "side": "RIGHT",
                        "body": confirmed_comment_body(record, verified, &j.flag.also_flagged),
                    })),
                    None => confirmed_general.push(bullet),
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
        lines.extend(render_partial_coverage_banner(&partial_reasons));
        if needs_check_count == 0 {
            lines.push(format!(
                "review ran: {} flags investigated across {} bundles, none confirmed. _{}_",
                env.deduped_flags,
                env.bundles,
                member_summary(env)
            ));
        } else {
            // "(not merge-blocking)" lives on the section header just below,
            // not repeated here too.
            lines.push(format!(
                "review ran: {} flags investigated across {} bundles, none confirmed — \
                 {} worth a double check. _{}_",
                env.deduped_flags,
                env.bundles,
                needs_check_count,
                member_summary(env)
            ));
            lines.push(String::new());
            lines.push("**Worth a double check** (not merge-blocking):".to_string());
            lines.extend(needs_check_section.iter().cloned());
        }
        lines.extend(run_warnings_block(env));
        lines.extend(size_cap_notice_lines(env));
        return Rendered {
            mode: if partial { "partial" } else { "comment" },
            review: None,
            comment: Some(format!("{}{}", lines.join("\n"), review_footer(env, attribution))),
        };
    }

    let mut body = vec!["### 🤖 PR review".to_string(), String::new()];
    body.extend(render_partial_coverage_banner(&partial_reasons));
    body.push(format!(
        "**Verdict: flag** · {} confirmed ({} inline, {} general)",
        confirmed_total,
        inline.len(),
        confirmed_general.len()
    ));
    // (#1521-adjacent UX) ONE header-level marker line when any confirmed
    // finding lacks a `verified` adjudication — replaces the old per-finding
    // repeat. A run where every confirmed finding IS verified emits nothing
    // here (each finding already names its adjudicator inline instead).
    if any_unverified {
        body.push(format!("_{CONFIRMED_MARKER}_"));
    }
    if !confirmed_general.is_empty() {
        body.push(String::new());
        body.push("**Confirmed findings not anchored to a diff line:**".to_string());
        body.extend(confirmed_general);
    }
    // (#1583) Cloned because the fallback body below renders the same
    // section — the two bodies are alternatives, never both posted.
    let needs_check_section_fallback = needs_check_section.clone();
    if needs_check_count > 0 {
        body.push(String::new());
        body.push("**Worth a double check** (not merge-blocking):".to_string());
        body.extend(needs_check_section);
    }
    body.extend(run_warnings_block(env));
    body.extend(size_cap_notice_lines(env));

    // (#1583) The summary-only fallback body, assembled from the same parts
    // but with EVERY confirmed finding as a prose bullet carrying its own
    // `path` — because in this rendering there are no inline comments to
    // carry the location. Posted by the workflow only when the formal review
    // is rejected; a successful review never shows it.
    //
    // It says plainly that anchoring was unavailable, so a summary-only
    // review can't be mistaken for a thin one — the same "never read as
    // something it isn't" contract `mode: degraded` carries (#1113).
    let mut fallback = vec!["### 🤖 PR review".to_string(), String::new()];
    fallback.extend(render_partial_coverage_banner(&partial_reasons));
    fallback.push(format!("**Verdict: flag** · {confirmed_total} confirmed"));
    fallback.push(
        "_Inline anchoring was unavailable for this run, so every finding is listed below with \
         its file. This lists every anchored AND unanchored confirmed finding — nothing is \
         dropped for lack of an anchor._"
            .to_string(),
    );
    if any_unverified {
        fallback.push(format!("_{CONFIRMED_MARKER}_"));
    }
    fallback.push(String::new());
    fallback.push("**Confirmed findings:**".to_string());
    fallback.extend(confirmed_all);
    if needs_check_count > 0 {
        fallback.push(String::new());
        fallback.push("**Worth a double check** (not merge-blocking):".to_string());
        fallback.extend(needs_check_section_fallback);
    }
    fallback.extend(run_warnings_block(env));
    fallback.extend(size_cap_notice_lines(env));

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
        mode: if partial { "partial" } else { "review" },
        review: Some(json!({
            "event": event,
            "body": format!("{}{}", body.join("\n"), review_footer(env, attribution)),
            "comments": inline,
        })),
        // (#1583) Present in `review` mode too, as the FALLBACK — not an
        // instruction to post it. `mode` still decides the default action;
        // this is only reached when posting the formal review fails.
        comment: Some(format!("{}{}", fallback.join("\n"), review_footer(env, attribution))),
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

/// (#1876/#1877) A PROMINENT banner for a partial-coverage run — deliberately
/// its own block, placed at the very TOP of the body/comment (never folded
/// into [`run_warnings_block`]'s small bullet list at the bottom), because a
/// coverage shortfall changes how the WHOLE posted review should be read,
/// not just one more thing to note in passing. `reasons` comes straight from
/// [`RunOutcome::Partial`] — already built from the envelope's own numbers
/// (`darkmux_lab::lab::review::review_outcome`), so this function never
/// invents a count of its own; a fixed string here would defeat the whole
/// point. Empty `reasons` (never reached by `synthesize_review`'s own call
/// site, but kept total rather than partial) renders nothing.
fn render_partial_coverage_banner(reasons: &[String]) -> Vec<String> {
    if reasons.is_empty() {
        return Vec::new();
    }
    let mut lines =
        vec![String::new(), "> [!WARNING]".to_string(), "> **Incomplete review — coverage shortfall:**".to_string()];
    lines.extend(reasons.iter().map(|r| format!("> - {r}.")));
    lines.push(
        "> The findings below are everything that WAS judged. This is not a clean pass — some work \
         never got a ruling."
            .to_string(),
    );
    lines.push(String::new());
    lines
}

/// The over-cap decline disclosure, appended to every posted review/comment
/// alongside [`run_warnings_block`] (#1605 established `env.bundle_skip` as
/// the bundler's own per-file decline accounting; this reads it for the ONE
/// reason it never covered outside the `bundles == 0` degenerate gate:
/// `SkipReason::OverSizeCap`/`TopLevelOverSizeCap`).
///
/// `env.bundle_skip` already reaches this function — it's a plain field on
/// `ReviewEnvelope`, stamped by `ReviewBundleStepKind::run_streaming`
/// alongside `bundles` itself, regardless of whether bundling produced any
/// bundles at all. But before this, nothing in the NORMAL render path (a
/// review with real findings, or a clean "none confirmed" pass) ever read
/// it: only `classify_zero_bundle_degenerate` looked at `bundle_skip`, and
/// only when `env.bundles == 0`. A PR that produced confirmed findings AND
/// had a function declined for size posted nothing about the decline — the
/// reviewer read a review that covered less than they thought, with no
/// marker at all.
///
/// Empty (and so a no-op on the caller's `lines.extend(...)`) when nothing
/// was declined for size — the common case, and what keeps this from
/// becoming noise on an ordinary review. Most `files_skipped` entries are
/// benign exclusions (`NonCodeExtension`, `TestFileExcluded`,
/// `SourceLanguageUnsupported`, …) that already have their own honest
/// framing elsewhere (`render_benign_noop_comment` /
/// `render_unsupported_language_comment`) or are simply not news on a
/// review that otherwise succeeded; only the two size-cap reasons mean
/// "part of the diff was too large to hand to a reviewer at all," which is
/// the one loss this disclosure exists to surface. This is disclosure, not
/// a gate — it never changes `mode`, `event`, or the confirmed/needs-check
/// counts.
fn size_cap_notice_lines(env: &ReviewEnvelope) -> Vec<String> {
    let Some(report) = &env.bundle_skip else {
        return Vec::new();
    };
    let mut declines: Vec<&SkippedFile> = report
        .files_skipped
        .iter()
        .filter(|f| matches!(f.reason, SkipReason::OverSizeCap | SkipReason::TopLevelOverSizeCap))
        .collect();
    if declines.is_empty() {
        return Vec::new();
    }
    declines.sort_by(|a, b| (a.path.as_str(), a.function.as_deref()).cmp(&(b.path.as_str(), b.function.as_deref())));
    let mut lines = vec![String::new(), "**⚠ Not reviewed — over the size limit**".to_string()];
    lines.extend(declines.iter().map(|f| format!("- {}", size_cap_decline_sentence(f))));
    lines
}

/// One decline as a sentence, e.g. `"Not reviewed: processOrder
/// (`src/orders.ts`, lines 88-2400) exceeds the size limit the review
/// system can process."` — see [`size_cap_notice_lines`].
///
/// `SkippedFile::function` is always `Some` for these two reasons (the
/// bundler records it the instant the function/run is skipped — see that
/// field's own doc), in the shape `"<label> (lines <a>-<b>)"` (both
/// `build_bundles` call sites use that exact `format!`). This splits the
/// line-span suffix off so it can be worded differently per reason, rather
/// than re-deriving the span from scratch — but falls back to a bare
/// "exceeds the size limit" sentence if that shape ever changes, instead of
/// panicking on a malformed report.
///
/// `TopLevelOverSizeCap`'s label is the literal `"toplevel"` — bundler-
/// internal jargon (see [`SkipReason::TopLevelOverSizeCap`]'s doc) that
/// must never leak verbatim into a comment posted on a public PR. This
/// reason has no enclosing function to name, so the sentence names the
/// file and line span only, with a plain-English note that it was a
/// top-level run rather than a single function.
fn size_cap_decline_sentence(f: &SkippedFile) -> String {
    let raw = f.function.as_deref().unwrap_or("");
    let span = raw
        .split_once(" (lines ")
        .map(|(_, rest)| format!("lines {}", rest.trim_end_matches(')')));
    match (f.reason, &span) {
        (SkipReason::TopLevelOverSizeCap, Some(span)) => format!(
            "Not reviewed: `{}` ({span}) exceeds the size limit the review system can process — \
             a run of top-level code, not inside a single function.",
            f.path
        ),
        (SkipReason::TopLevelOverSizeCap, None) => format!(
            "Not reviewed: `{}` exceeds the size limit the review system can process — a run of \
             top-level code, not inside a single function.",
            f.path
        ),
        (_, Some(span)) => {
            let name = raw.split_once(" (lines ").map(|(n, _)| n).unwrap_or(raw);
            format!(
                "Not reviewed: {name} (`{}`, {span}) exceeds the size limit the review system can process.",
                f.path
            )
        }
        (_, None) => format!(
            "Not reviewed: `{}` exceeds the size limit the review system can process.",
            f.path
        ),
    }
}

/// (#1260) The frontier-verified line a `verified` adjudication earns —
/// names the adjudicating model on THIS finding, so the posted comment says
/// WHERE the verification came from (operator sovereignty: the reader never
/// wonders which tier signed off). An unverified finding renders no
/// per-finding counterpart — the header-level [`CONFIRMED_MARKER`] covers
/// it once for the whole run instead (see `any_unverified` in
/// [`synthesize_review`]).
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
    if let Some(v) = verified {
        lines.push(verified_line(v));
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
    if let Some(v) = verified {
        line.push_str(&format!(" ({})", verified_line(v)));
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
    // (#1605) Test-only — the no-op-comment tests build a `bundle_skip`
    // report by hand; production code only ever receives one already
    // populated by `ReviewBundleStepKind::run_streaming`.
    use darkmux_lab::lab::bundle::{BundleSkipReport, SkipReason, SkippedFile};
    // (#1876/#1877) Test-only — the partial-coverage tests build a
    // `remote_budgets` row by hand; production code only ever reads one
    // already populated by `judge_gate_outcome`/`RemoteBudget::record`.
    use darkmux_crew::remote_budget::RemoteBudgetRecord;

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
                absence_backstop: None,
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
                    absence_backstop: None,
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
                    absence_backstop: None,
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
                absence_backstop: None,
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
    fn synthesize_confirmed_resolves_inline_with_header_marker_and_comment_by_default() {
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
        // (#1521-adjacent UX) The marker is no longer repeated on each
        // finding's own comment — only the review's top-level body carries it.
        assert!(!body.contains(CONFIRMED_MARKER), "the marker moved to the header, not the finding: {body}");
        let note_at = body.find("shadows the config default").unwrap();
        let evidence_at = body.find("the clamp is bypassed").unwrap();
        assert!(note_at < evidence_at, "{body}");

        let top_body = review["body"].as_str().unwrap();
        assert!(
            top_body.contains(CONFIRMED_MARKER),
            "the run has one unverified confirm, so the header marker fires once: {top_body}"
        );
    }

    /// (#1583) A `review`-mode render also carries a summary-only FALLBACK
    /// body, so a rejected inline post degrades instead of discarding the
    /// run. The failure this pins: a real review with eight confirmed
    /// findings — one of them a genuine inverted-comparator bug — left NO
    /// trace on the PR because a 422 on the anchors killed the posting step.
    ///
    /// The load-bearing property is that the fallback carries findings the
    /// review body does NOT: an anchored finding lives only in
    /// `review.comments`, so a fallback built from `review.body` alone would
    /// silently drop exactly the findings that anchored successfully.
    #[test]
    fn synthesize_review_mode_also_renders_a_summary_only_fallback() {
        let anchored = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "shadows the config default",
            "the clamp is bypassed",
        );
        // Deliberately unresolvable, so this one lands in the general body.
        let unanchored = confirmed_flag(
            "other@src/x.ts",
            Some("no-such-line-anywhere"),
            "second finding note",
            "second finding evidence",
        );
        let env = healthy_envelope(vec![anchored, unanchored]);
        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "review", "the fallback never changes the default action");

        let review = r.review.as_ref().unwrap();
        assert_eq!(review["comments"].as_array().unwrap().len(), 1, "one finding anchored");

        let fallback = r.comment.as_deref().expect("review mode must render a fallback body");
        // BOTH findings, including the one that only existed as an inline
        // comment — the whole point.
        assert!(
            fallback.contains("shadows the config default"),
            "the ANCHORED finding must survive into the fallback: {fallback}"
        );
        assert!(
            fallback.contains("second finding note"),
            "the unanchored finding must too: {fallback}"
        );
        // The anchored finding's location has to come along, since there is
        // no inline comment to carry it in this rendering.
        assert!(fallback.contains("src/x.ts"), "findings must name their file: {fallback}");
        // Says plainly it is summary-only, so it can't be misread as thin.
        assert!(
            fallback.contains("Inline anchoring was unavailable"),
            "a summary-only review must say so: {fallback}"
        );
        assert!(fallback.contains("2 confirmed"), "the count must be honest: {fallback}");
    }

    /// (#1583) A `comment`-mode render is unaffected — it had a `comment`
    /// body before and still does, and gains no `review` payload.
    #[test]
    fn comment_mode_is_unchanged_by_the_fallback_work() {
        let env = healthy_envelope(vec![]);
        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "comment", "zero confirms is still a plain comment");
        assert!(r.review.is_none());
        assert!(r.comment.is_some());
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

    /// (#1605) A benign-empty run — every file the diff touched declined
    /// because it was non-code content — posts a NEUTRAL no-op comment
    /// (`mode: "noop"`), never the loud "degraded"/"no signal" treatment,
    /// and the comment names what the diff actually contained.
    #[test]
    fn synthesize_benign_empty_envelope_is_a_noop_naming_what_the_diff_contained() {
        let env = ReviewEnvelope {
            degenerate: Some(
                "no bundles produced from the diff — 2 file(s) considered, 2 skipped \
                 (2 non-code extension)"
                    .to_string(),
            ),
            degenerate_kind: Some(DegenerateKind::BenignEmpty),
            bundle_skip: Some(BundleSkipReport {
                files_considered: 2,
                files_skipped: vec![
                    SkippedFile {
                        path: "package-lock.json".to_string(),
                        reason: SkipReason::NonCodeExtension,
                        function: None,
                    },
                    SkippedFile {
                        path: "fixtures/sample.json".to_string(),
                        reason: SkipReason::NonCodeExtension,
                        function: None,
                    },
                ],
            }),
            ..Default::default()
        };
        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "noop", "a benign-empty run must post the neutral noop mode, never degraded");
        assert!(r.review.is_none(), "never a formal review payload for a no-op");
        let c = r.comment.expect("a noop run still posts a comment");
        // Names WHAT the diff contained — the actual file paths, not just a
        // bare "nothing to review" — so the requester sees the diff was
        // genuinely inspected, not silently dropped.
        assert!(c.contains("package-lock.json"), "{c}");
        assert!(c.contains("fixtures/sample.json"), "{c}");
        assert!(c.contains("2 file(s)"), "the count is named too: {c}");
        // Explicitly NOT an approval — a neutral note, and never the
        // "degraded"/"no review signal" red-failure language.
        assert!(
            !c.to_lowercase().contains("no signal") && !c.to_lowercase().contains("degraded"),
            "a benign-empty note must never read like the loud degraded failure comment: {c}"
        );
        assert!(
            c.to_lowercase().contains("neutral") || c.to_lowercase().contains("not an approval"),
            "must explicitly disclaim this is not an approval: {c}"
        );
        // (#1605 operator direction) The comment's PRIMARY audience is the
        // agent session waiting on this review, not a human skimming the PR.
        // A session that can't tell "nothing to review" from "broken, try
        // again" re-runs — and a benign diff becomes a retry LOOP, since
        // every attempt produces the identical empty result. So the comment
        // has to say, in words, that the bundler worked and that re-running
        // changes nothing. Asserted rather than left to prose drift: this is
        // now the load-bearing sentence in the whole message.
        let lower = c.to_lowercase();
        assert!(
            lower.contains("worked as expected") || lower.contains("not a failure"),
            "the comment must state the bundler WORKED — otherwise a waiting session reads \
             an empty result as breakage: {c}"
        );
        assert!(
            lower.contains("re-running will produce the same result")
                || lower.contains("re-run"),
            "the comment must tell a waiting session NOT to retry — an unbounded retry loop \
             on a permanently-empty diff is the failure mode this wording exists to prevent: {c}"
        );
    }

    /// (#1757) A diff whose zero-bundle result is real source in a
    /// language the built-in bundler can't parse posts a NEUTRAL noop
    /// comment too (never fails the check), but with different wording
    /// than the benign-empty case: it names the file count + extensions
    /// and points at the `--bundler` guide, rather than saying "nothing to
    /// review" about real code.
    #[test]
    fn synthesize_unsupported_language_envelope_is_a_noop_naming_the_bundler_escape_hatch() {
        let env = ReviewEnvelope {
            degenerate: Some(
                "no bundles produced from the diff — 2 file(s) considered, 2 skipped \
                 (1 non-code extension, 1 real source in an unsupported language)"
                    .to_string(),
            ),
            degenerate_kind: Some(DegenerateKind::UnsupportedLanguage),
            bundle_skip: Some(BundleSkipReport {
                files_considered: 2,
                files_skipped: vec![
                    SkippedFile {
                        path: "package-lock.json".to_string(),
                        reason: SkipReason::NonCodeExtension,
                        function: None,
                    },
                    SkippedFile {
                        path: "migrations/001_add_users.sql".to_string(),
                        reason: SkipReason::SourceLanguageUnsupported,
                        function: None,
                    },
                ],
            }),
            ..Default::default()
        };
        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "noop", "must never fail the check — a neutral outcome, not degraded");
        assert!(r.review.is_none(), "never a formal review payload for a no-op");
        let c = r.comment.expect("a noop run still posts a comment");
        // Names the unsupported-language file, its extension, and the count
        // — never lumped in with the benign lockfile skip.
        assert!(c.contains("migrations/001_add_users.sql"), "{c}");
        assert!(c.contains(".sql"), "the extension must be named: {c}");
        assert!(c.contains('1'), "the unsupported-language file count is named: {c}");
        // Points at the escape hatch's own guide page.
        assert!(
            c.contains("darkmux.com/guide/bundlers.html"),
            "must point at the bundler guide so the requester's next step is a link: {c}"
        );
        assert!(
            c.to_lowercase().contains("--bundler") || c.to_lowercase().contains("bring your own bundler"),
            "must name the escape hatch itself, not just link to it: {c}"
        );
        // Must NOT read as the benign "nothing to review" case — there IS
        // real code here.
        let lower = c.to_lowercase();
        assert!(
            !lower.contains("nothing here for an automated code review to read"),
            "an unsupported-language diff has REAL code — must not use the benign-empty wording: {c}"
        );
        assert!(
            !lower.contains("no signal") && !lower.contains("degraded"),
            "must never read like the loud degraded failure comment: {c}"
        );
        assert!(
            lower.contains("neutral") || lower.contains("not an approval"),
            "must explicitly disclaim this is not an approval: {c}"
        );
    }

    // ─── size-cap decline disclosure — the normal review path ─────────────
    //
    // `env.bundle_skip` already reaches `synthesize_review` (it's a plain
    // field on `ReviewEnvelope`, stamped by `ReviewBundleStepKind::
    // run_streaming` alongside `bundles` itself — see that field's own
    // doc in review.rs). But before this, NOTHING in the normal render
    // path ever read it for a size-cap decline: only
    // `classify_zero_bundle_degenerate` looked at `bundle_skip`, and only
    // when `env.bundles == 0`. A review that produced real findings (or a
    // clean "none confirmed" result) alongside a
    // `SkipReason::OverSizeCap`/`TopLevelOverSizeCap` decline posted
    // NOTHING about it — the human reviewer read a review that covered
    // less than they thought, with no marker at all. These tests pin the
    // fix at the render layer; no envelope plumbing was needed.

    fn over_cap_skip(considered: usize, path: &str, function_label: &str) -> BundleSkipReport {
        BundleSkipReport {
            files_considered: considered,
            files_skipped: vec![SkippedFile {
                path: path.to_string(),
                reason: SkipReason::OverSizeCap,
                function: Some(function_label.to_string()),
            }],
        }
    }

    #[test]
    fn synthesize_review_with_findings_names_an_over_cap_decline() {
        let j = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "shadows the config default",
            "the clamp is bypassed",
        );
        let mut env = healthy_envelope(vec![j]);
        env.bundle_skip = Some(over_cap_skip(2, "src/orders.ts", "processOrder (lines 88-2400)"));

        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "review", "a size decline must not change the pass/fail outcome");

        let body = r.review.as_ref().unwrap()["body"].as_str().unwrap().to_string();
        assert!(body.contains("processOrder"), "must name the dropped function: {body}");
        assert!(body.contains("src/orders.ts"), "must name the file: {body}");
        assert!(body.contains("88") && body.contains("2400"), "must name the line span: {body}");
        assert!(
            body.to_lowercase().contains("size limit") || body.to_lowercase().contains("size cap"),
            "must say the reason was size: {body}"
        );
        assert!(body.contains("Not reviewed"), "{body}");

        // The summary-only fallback (posted when GitHub rejects the inline
        // anchors) is an alternate rendering of the SAME run — it must
        // carry the same disclosure, not just the formal review body.
        let fallback = r.comment.unwrap();
        assert!(fallback.contains("processOrder"), "{fallback}");
        assert!(fallback.contains("src/orders.ts"), "{fallback}");
    }

    /// The inverted case: a `bundle_skip` report that carries declines, but
    /// NONE of them are size-related, must add nothing to the render — same
    /// output as no `bundle_skip` at all. This is what stops the notice
    /// from becoming noise on an ordinary review (most declines are benign
    /// non-code/test-file exclusions, not size drops).
    #[test]
    fn synthesize_review_with_findings_and_no_size_declines_renders_unchanged() {
        let j = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "shadows the config default",
            "the clamp is bypassed",
        );

        let env_no_skip = healthy_envelope(vec![j.clone()]);
        let r_no_skip = synthesize_review(&env_no_skip, DIFF, None);

        let mut env_benign_skip = healthy_envelope(vec![j]);
        env_benign_skip.bundle_skip = Some(BundleSkipReport {
            files_considered: 2,
            files_skipped: vec![SkippedFile {
                path: "package-lock.json".to_string(),
                reason: SkipReason::NonCodeExtension,
                function: None,
            }],
        });
        let r_benign_skip = synthesize_review(&env_benign_skip, DIFF, None);

        assert_eq!(
            r_no_skip.review.as_ref().unwrap()["body"],
            r_benign_skip.review.as_ref().unwrap()["body"],
            "a bundle_skip report with no size-cap declines must render byte-identically \
             to no bundle_skip at all"
        );
        assert_eq!(
            r_no_skip.comment, r_benign_skip.comment,
            "...and so must the fallback comment"
        );
        let body = r_no_skip.review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(!body.to_lowercase().contains("not reviewed"), "no size decline: {body}");
    }

    #[test]
    fn synthesize_review_names_a_top_level_over_cap_decline_without_a_function_name() {
        let j = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "shadows the config default",
            "the clamp is bypassed",
        );
        let mut env = healthy_envelope(vec![j]);
        env.bundle_skip = Some(BundleSkipReport {
            files_considered: 1,
            files_skipped: vec![SkippedFile {
                path: "src/imports.ts".to_string(),
                reason: SkipReason::TopLevelOverSizeCap,
                function: Some("toplevel (lines 1-2500)".to_string()),
            }],
        });

        let r = synthesize_review(&env, DIFF, None);
        let body = r.review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(body.contains("src/imports.ts"), "must name the file: {body}");
        assert!(body.contains('1') && body.contains("2500"), "must name the line span: {body}");
        assert!(
            body.to_lowercase().contains("size limit") || body.to_lowercase().contains("size cap"),
            "{body}"
        );
        // No enclosing function exists for a top-level run — the bundler's
        // internal "toplevel" label (`SkippedFile::function`'s own doc)
        // must never leak verbatim into a comment posted on a public PR.
        assert!(!body.contains("toplevel"), "must not leak the internal label: {body}");
    }

    #[test]
    fn synthesize_review_renders_multiple_size_declines_readably() {
        let j = confirmed_flag(
            "computeEnd@src/x.ts",
            Some("const b = 2;"),
            "shadows the config default",
            "the clamp is bypassed",
        );
        let mut env = healthy_envelope(vec![j]);
        env.bundle_skip = Some(BundleSkipReport {
            files_considered: 3,
            files_skipped: vec![
                SkippedFile {
                    path: "src/orders.ts".to_string(),
                    reason: SkipReason::OverSizeCap,
                    function: Some("processOrder (lines 88-2400)".to_string()),
                },
                SkippedFile {
                    path: "src/imports.ts".to_string(),
                    reason: SkipReason::TopLevelOverSizeCap,
                    function: Some("toplevel (lines 1-2500)".to_string()),
                },
            ],
        });

        let r = synthesize_review(&env, DIFF, None);
        let body = r.review.unwrap()["body"].as_str().unwrap().to_string();
        assert!(body.contains("processOrder") && body.contains("src/orders.ts"), "{body}");
        assert!(body.contains("src/imports.ts"), "{body}");
        let notice_lines: Vec<&str> =
            body.lines().filter(|l| l.starts_with("- Not reviewed")).collect();
        assert_eq!(
            notice_lines.len(),
            2,
            "each decline gets its own line, not merged into one: {body}\n{notice_lines:?}"
        );
    }

    /// Requirement 1's "or a clean result" half — a zero-confirmed run
    /// (`mode: "comment"`) with a size decline must also disclose it, not
    /// just the `mode: "review"` findings path.
    #[test]
    fn synthesize_zero_confirms_clean_result_also_names_an_over_cap_decline() {
        let env = ReviewEnvelope {
            deduped_flags: 3,
            bundles: 2,
            judged: vec![archived_flag("computeEnd@src/x.ts")],
            members: vec![
                MemberRecord { model: "darkmux:probe-a".into(), seat: "review-probe".into(), ..Default::default() },
                MemberRecord { model: "darkmux:judge-b".into(), seat: "review-judge".into(), ..Default::default() },
            ],
            bundle_skip: Some(over_cap_skip(2, "src/orders.ts", "processOrder (lines 88-2400)")),
            ..Default::default()
        };
        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "comment");
        let c = r.comment.unwrap();
        assert!(c.contains("processOrder") && c.contains("src/orders.ts"), "{c}");
        assert!(c.to_lowercase().contains("size limit") || c.to_lowercase().contains("size cap"), "{c}");
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
                absence_backstop: None,
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

    /// (#1676) With NO member records the envelope holds no evidence that
    /// anything ran, so the footer must make no claim about where it ran. It
    /// used to say "on a self-hosted runner" — a hardcoded environment claim
    /// in the one branch with zero evidence, on a surface whose whole job is
    /// audit integrity. A v2.5.0 dogfood launched from a laptop shell and got
    /// exactly that sentence posted.
    ///
    /// The tagline, the darkmux attribution, and the rest of the footer must
    /// survive — this drops a false clause, it doesn't drop the footer.
    #[test]
    fn footer_with_no_member_records_claims_no_execution_environment() {
        let body = footer_body_for(vec![]);
        assert!(
            !body.contains("self-hosted runner"),
            "an envelope with no member records must not claim a runner: {body}"
        );
        for claim in ["no cloud API", "hosted cloud endpoint", "mixed crew", "on a local model"] {
            assert!(
                !body.contains(claim),
                "no member records ⇒ no provenance claim, but found {claim:?}: {body}"
            );
        }
        assert!(body.contains(REVIEW_TAGLINE), "the tagline still renders: {body}");
        assert!(body.contains("Powered by darkmux"), "the attribution still renders: {body}");
        assert!(body.contains("Automated review"), "the footer is still a footer: {body}");
    }

    /// The inverted case, so the test above cannot pass merely because the
    /// footer stopped rendering provenance for everyone: an envelope that DOES
    /// carry a member record still names where that seat ran.
    #[test]
    fn footer_with_a_member_record_still_names_where_it_ran() {
        let body = footer_body_for(vec![judge_member("darkmux:qwen-judge", false)]);
        assert!(
            body.contains("on a local model, no cloud API"),
            "evidence present ⇒ the clause must still render: {body}"
        );
        assert!(body.contains("darkmux:qwen-judge"), "and name the model: {body}");
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
        // (#1521-adjacent UX) An unverified finding's own body no longer
        // carries the marker — that now lives on the review header only.
        assert!(!body.contains(CONFIRMED_MARKER), "{body}");
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

    /// (#1260) An `uncertain` adjudication keeps the finding unverified —
    /// inconclusive never promotes; the posted bytes match a no-seat crew,
    /// and the header-level marker still fires for both.
    #[test]
    fn synthesize_uncertain_verify_keeps_the_header_marker() {
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
        assert!(
            a["body"].as_str().unwrap().contains(CONFIRMED_MARKER),
            "the sole finding is unverified, so the header marker fires once"
        );
    }

    /// (FIX 3 / #1260, ruling applied) A verify budget that exhausts
    /// MID-STAGE degrades the STAGE, not the run: verified findings still post
    /// as frontier-verified, the skipped adjudication (recorded per-flag as
    /// `VerifyRuling::Error`, tier still Confirmed) leaves the run with an
    /// unverified confirm — so the header-level marker fires once — and the
    /// posted review carries the loud "verify budget exhausted after N of M
    /// adjudications" warning. The envelope is NOT degenerate — never routed
    /// to "produced no signal".
    #[test]
    fn synthesize_verify_exhaustion_posts_verified_plus_header_marker_plus_warning() {
        let mut verified = confirmed_flag("a@src/x.ts", Some("const b = 2;"), "verified note", "e");
        verified.verify = Some(verify_record(VerifyRuling::Verified));
        // A skipped adjudication: recorded per-flag as Error, stays Confirmed
        // — leaves the run with an unverified confirm.
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
        let body = review["body"].as_str().unwrap();
        assert!(
            body.contains(CONFIRMED_MARKER),
            "the skipped adjudication leaves an unverified confirm, firing the header marker once: {body}"
        );
        assert!(body.contains("Run warnings"), "the warnings block renders on the review: {body}");
        assert!(body.contains("verify budget exhausted after 1 of 2 adjudications"), "{body}");
    }

    /// (#1876/#1877) The exact production incident shape: a judge stage
    /// that ruled 134 flags — 7 confirmed, 67 needs-check, 60 archived —
    /// and a `judge-pass1` remote-budget row naming 11 skipped calls after
    /// the per-execution allowance ran out. BEFORE the fix, this exact
    /// envelope (`env.degenerate` set by the old unconditional Gate 1)
    /// rendered "the review produced no signal" and discarded every one of
    /// the 7 confirmed findings; the fix leaves `env.degenerate` unset for
    /// this shape (a `judge_gate_outcome` unit-level concern, pinned in
    /// `crates/darkmux-lab/src/lab/review_tests.rs`) and this test pins the
    /// RENDER side: the findings post normally, plus a banner.
    #[test]
    fn synthesize_review_with_partial_judge_coverage_renders_findings_and_a_banner() {
        let mut judged: Vec<JudgedFlag> = Vec::new();
        for i in 0..7 {
            judged.push(confirmed_flag(
                &format!("fn{i}@src/x.ts"),
                None,
                &format!("confirmed note {i}"),
                &format!("evidence {i}"),
            ));
        }
        for i in 0..67 {
            judged.push(needs_check_flag(&format!("fn{i}@src/y.ts"), None, &format!("needs-check note {i}"), false));
        }
        for i in 0..60 {
            judged.push(archived_flag(&format!("fn{i}@src/z.ts")));
        }
        assert_eq!(judged.len(), 134);
        let mut env = healthy_envelope(judged);
        env.remote_budgets = vec![RemoteBudgetRecord {
            stage: "judge-pass1".to_string(),
            max_tokens: 500_000,
            used_tokens: 500_497,
            exhausted: true,
            skipped_calls: 11,
        }];

        let r = synthesize_review(&env, DIFF, None);
        assert_ne!(r.mode, "degraded", "usable rulings exist — must never discard as no signal");
        assert_eq!(r.mode, "partial", "a coverage shortfall with real signal renders as the partial case");
        let review = r.review.expect("7 confirmed findings still produce a formal review payload");
        let body = review["body"].as_str().unwrap();
        assert!(body.contains("Incomplete review"), "the banner is present: {body}");
        assert!(body.contains("11 of 134 flags went unjudged"), "banner names the envelope's real numbers: {body}");
        assert!(body.contains("confirmed note 0"), "confirmed findings still render: {body}");
        assert!(body.contains("7 confirmed"), "the verdict line still counts confirmed findings: {body}");
        assert!(
            body.contains("Confirmed findings not anchored to a diff line"),
            "the 7 unanchored confirms still render in the general section: {body}"
        );
        let fallback = r.comment.expect("the summary-only fallback is still built");
        assert!(fallback.contains("Incomplete review"), "the banner also carries into the fallback: {fallback}");
    }

    /// (#1876) The banner is built from THIS envelope's own numbers, never
    /// a fixed string — deliberately different numbers than the production
    /// shape above (4 of 9, not 11 of 134), so a hardcoded banner string
    /// would fail this test even though it passed that one.
    #[test]
    fn synthesize_review_partial_banner_names_the_envelopes_own_numbers_never_a_fixed_string() {
        let mut judged: Vec<JudgedFlag> = Vec::new();
        for i in 0..3 {
            judged.push(confirmed_flag(&format!("fn{i}@src/x.ts"), None, &format!("note {i}"), "evidence"));
        }
        for i in 0..6 {
            judged.push(archived_flag(&format!("fn{i}@src/z.ts")));
        }
        assert_eq!(judged.len(), 9);
        let mut env = healthy_envelope(judged);
        env.remote_budgets = vec![RemoteBudgetRecord {
            stage: "judge-pass1".to_string(),
            max_tokens: 1_000,
            used_tokens: 999,
            exhausted: true,
            skipped_calls: 4,
        }];

        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "partial");
        let review = r.review.unwrap();
        let body = review["body"].as_str().unwrap();
        assert!(body.contains("4 of 9 flags went unjudged"), "{body}");
        assert!(!body.contains("11 of 134"), "must not accidentally carry another fixture's numbers: {body}");
    }

    /// (#1876/#1877 QA follow-up) The zero-confirmed side of `partial` —
    /// needs-check findings only, a judge-pass1 skip row, no confirmed
    /// findings at all. This is the branch the workflow's `partial)` case
    /// has to handle differently (`jq -r '.review // "null"'` sees `null`
    /// and posts a plain comment instead of attempting a formal review),
    /// and it's exactly what the wording-fix means will actually happen in
    /// practice: a pass-2 budget exhaustion demotes confirms to
    /// needs-check, which can easily zero out `confirmed_total` on its own.
    #[test]
    fn synthesize_review_partial_with_zero_confirmed_uses_the_comment_shape() {
        let mut judged: Vec<JudgedFlag> = Vec::new();
        for i in 0..5 {
            judged.push(needs_check_flag(&format!("fn{i}@src/y.ts"), None, &format!("needs-check note {i}"), false));
        }
        let mut env = healthy_envelope(judged);
        env.remote_budgets = vec![RemoteBudgetRecord {
            stage: "judge-pass1".to_string(),
            max_tokens: 1_000,
            used_tokens: 900,
            exhausted: true,
            skipped_calls: 2,
        }];

        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "partial", "a coverage shortfall with zero confirmed findings is still partial");
        assert!(r.review.is_none(), "no formal review payload without any confirmed findings");
        let comment = r.comment.expect("the comment-shape payload is still built");
        assert!(comment.contains("Incomplete review"), "the banner renders in comment mode too: {comment}");
        assert!(comment.contains("2 of 5 flags went unjudged"), "{comment}");
        assert!(comment.contains("needs-check note 0"), "the needs-check findings still render: {comment}");
    }

    /// (#1876/#1877 QA follow-up) `review_outcome`'s Empty case wins over a
    /// skip row when BOTH are present on the same envelope — e.g. Gate 2
    /// fired (zero usable rulings) on a run that ALSO carries a
    /// `remote_budgets` skip row from the same exhausted bucket.
    /// `synthesize_review` must still route to `"degraded"` and discard,
    /// never to `"partial"` — a skip row is only evidence for Partial when
    /// there is no `degenerate` reason set at all.
    #[test]
    fn synthesize_review_degenerate_wins_over_a_present_skip_row() {
        let mut env = ReviewEnvelope {
            degenerate: Some(
                "remote judge token budget exhausted — 5 judge call(s) skipped after the \
                 per-execution allowance (100 tokens per stage) ran out, and none of the flags \
                 that WERE judged produced a usable ruling — degenerate run, never a silent pass"
                    .to_string(),
            ),
            remote_budgets: vec![RemoteBudgetRecord {
                stage: "judge-pass1".to_string(),
                max_tokens: 100,
                used_tokens: 100,
                exhausted: true,
                skipped_calls: 5,
            }],
            ..Default::default()
        };
        // A degenerate envelope also carries a real skip row on the same
        // stage — the precedence under test.
        env.judged = Vec::new();

        let r = synthesize_review(&env, DIFF, None);
        assert_eq!(r.mode, "degraded", "Empty must win over a present-but-irrelevant skip row");
        let c = r.comment.unwrap();
        assert!(c.contains("no signal"), "{c}");
        assert!(!c.contains("Incomplete review"), "the partial banner must never render alongside a discard: {c}");
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
        // (#1521-adjacent UX) The per-bullet marker moved to the review header.
        assert!(!line.contains(CONFIRMED_MARKER), "{line}");
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
        // (#1521-adjacent UX) The per-finding marker moved to the review
        // header; the top-level body still carries it once.
        assert!(!body.contains(CONFIRMED_MARKER), "{body}");
        let top_body = review["body"].as_str().unwrap();
        assert!(top_body.contains(CONFIRMED_MARKER), "{top_body}");
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
